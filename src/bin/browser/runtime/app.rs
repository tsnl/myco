//! Publishing the live projection: snapshot, metadata, and block updates.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use chrono::Utc;
use myco::session::ActiveSession;
use myco::{AgentEvent, EventSink};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    App, Block, DownHost, Error, Result, SessionFacts, Snapshot, Status, Update, attachments,
    service, view,
};
use crate::Boot;

/// Largest rebuilt-history tail relayed to every browser instead of a refetch.
const MAX_SPLICE_BYTES: usize = 64 * 1024;

/// The changed tail of a rebuilt history on the same thread, with every
/// non-block field, or `None` when browsers should refetch it.
fn splice(previous: &[Block], snapshot: &mut Snapshot) -> Option<Value> {
    let visible = |block: &Block| serde_json::to_value(block).ok();
    let start = previous
        .iter()
        .zip(&snapshot.blocks)
        .take_while(|(old, new)| visible(old) == visible(new))
        .count();
    let tail = serde_json::to_value(&snapshot.blocks[start..]).ok()?;
    let blocks = std::mem::take(&mut snapshot.blocks);
    let head = serde_json::to_value(&*snapshot);
    snapshot.blocks = blocks;
    let splice = json!({"start": start, "blocks": tail, "snapshot": head.ok()?});
    (serde_json::to_vec(&splice).ok()?.len() <= MAX_SPLICE_BYTES).then_some(splice)
}

impl App {
    pub(crate) fn background(&self, session_id: &str, call_id: Uuid) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        if live.snapshot.meta.session_id != session_id || live.snapshot.meta.cancelling() {
            return Err(Error::Conflict(
                "The session changed or is cancelling.".into(),
            ));
        }
        let token = live.background.get(&call_id).ok_or_else(|| {
            Error::Conflict("This tool call is no longer running or cannot be backgrounded.".into())
        })?;
        token.cancel();
        if let Some(index) = live.snapshot.blocks.iter().position(|block| matches!(block, Block::Tool { call_id: id, running: true, .. } if *id == call_id)) {
            if let Block::Tool { background_id, status, .. } = &mut live.snapshot.blocks[index] {
                *background_id = None;
                *status = "backgrounding".into();
            }
            self.publish_block(&mut live.snapshot, index);
        }
        Ok(())
    }

    pub(super) fn stop(&self) {
        self.shutdown.cancel();
        if let Some(cancel) = &self.live.lock().unwrap().snapshot.meta.cancel {
            cancel.cancel();
        }
    }

    fn publish(&self, snapshot: &mut Snapshot, change: Value) {
        snapshot.revision += 1;
        let _ = self.events.send(Arc::new(Update {
            session_id: snapshot.meta.session_id.clone(),
            revision: snapshot.revision,
            change,
        }));
    }

    /// Publish a block that changed in place.
    fn publish_block(&self, snapshot: &mut Snapshot, index: usize) {
        let change = json!({"kind":"block", "index":index, "block":snapshot.blocks[index]});
        self.publish(snapshot, change);
    }

    /// Add a block to the end of the transcript.
    pub(super) fn push_block(&self, snapshot: &mut Snapshot, block: Block) {
        snapshot.blocks.push(block);
        self.publish_block(snapshot, snapshot.blocks.len() - 1);
    }

    /// Publish a rebuilt history. Browsers apply a bounded splice of the same
    /// thread in place; anything else invalidates their copy.
    fn publish_history(&self, snapshot: &mut Snapshot, previous: Option<&[Block]>) {
        let splice = previous.and_then(|previous| splice(previous, snapshot));
        let mut change = json!({"kind":"snapshot", "snapshot":snapshot});
        if let Some(splice) = splice {
            change["splice"] = splice;
        }
        self.publish(snapshot, change);
    }

    pub(super) fn publish_metadata(&self, snapshot: &mut Snapshot) {
        let change = json!({"kind":"meta", "meta":snapshot.meta});
        self.publish(snapshot, change);
    }

    pub(crate) fn snapshot(&self) -> Update {
        let live = self.live.lock().unwrap();
        Update {
            session_id: live.snapshot.meta.session_id.clone(),
            revision: live.snapshot.revision,
            change: json!({"kind":"snapshot", "snapshot":live.snapshot}),
        }
    }

    pub(super) fn notice(&self, text: impl Into<String>) {
        self.append_block(Block::Notice { text: text.into() });
    }

    pub(super) fn compacting(&self) {
        self.status(Status::Compacting);
    }

    pub(super) fn finish_compaction(&self) {
        self.append_block(Block::Boundary {
            time: view::timestamp(&Utc::now()),
        });
        self.status(Status::Running);
    }

    pub(super) fn warning(&self, text: String) {
        let compacting = self.live.lock().unwrap().snapshot.meta.status == Status::Compacting;
        if compacting {
            self.status(Status::Running);
        }
        self.notice(text);
    }

    pub(super) fn retrying(&self, error: String, delay: Duration) {
        self.discard_generation();
        self.status(Status::Retrying);
        self.retry_notice(format!(
            "{error} — auto-continue retrying in {:.1}s",
            delay.as_secs_f64()
        ));
    }

    pub(super) fn status(&self, status: Status) {
        let mut live = self.live.lock().unwrap();
        if live.snapshot.meta.progress(status) {
            self.publish_metadata(&mut live.snapshot);
        }
    }

    pub(super) fn append_block(&self, block: Block) {
        self.push_block(&mut self.live.lock().unwrap().snapshot, block);
    }

    fn retry_notice(&self, text: String) {
        let mut live = self.live.lock().unwrap();
        let block = Block::Notice { text };
        if let Some(index) = live.retry_notice {
            live.snapshot.blocks[index] = block;
            self.publish_block(&mut live.snapshot, index);
        } else {
            live.retry_notice = Some(live.snapshot.blocks.len());
            self.push_block(&mut live.snapshot, block);
        }
    }

    fn discard_generation(&self) {
        let mut live = self.live.lock().unwrap();
        let Some(start) = live.generation_start.take() else {
            return;
        };
        // A finished generation may still need its checkpoint repaired. Its
        // validated service output must survive until GenerationCommitted.
        live.service.reset_draft();
        let previous = live.snapshot.blocks.clone();
        let mut index = 0;
        let retry_notice = live.retry_notice;
        let mut removed_before_notice = 0;
        // Resource refreshes may append live tool cards while a draft is streaming.
        live.snapshot.blocks.retain(|block| {
            let keep = index < start
                || !matches!(block, Block::Message { role, .. } if role == "assistant" || role == "thinking");
            if !keep && retry_notice.is_some_and(|notice| index < notice) {
                removed_before_notice += 1;
            }
            index += 1;
            keep
        });
        live.retry_notice = retry_notice.map(|index| index - removed_before_notice);
        if live.snapshot.blocks.len() == previous.len() {
            return;
        }
        self.publish_history(&mut live.snapshot, Some(&previous));
    }

    pub(super) fn sync_with(
        &self,
        boot: &Boot,
        status: Status,
        finish: impl FnOnce(&mut service::Receipts),
    ) {
        let session = boot.session.snapshot();
        let mut live = self.live.lock().unwrap();
        // The receipt and snapshot settle under one lock. A completed native
        // command cannot expose stale settings or cancel a subsequent turn.
        finish(&mut live.service);
        live.generation_start = None;
        live.retry_notice = None;
        let snapshot = &mut live.snapshot;
        let same_thread = snapshot.meta.session_id == session.id
            && snapshot.meta.thread_id == session.active_thread().id;
        let mut blocks = view::history(session.active_thread());
        if snapshot.meta.thread_id == session.active_thread().id {
            view::retain_tool_timers(&mut blocks, &snapshot.blocks);
        }
        view::retain_processes(&mut blocks, &snapshot.blocks);
        let previous = same_thread.then(|| std::mem::take(&mut snapshot.blocks));
        snapshot.meta.session_id = session.id.clone();
        snapshot.meta.thread_id = session.active_thread().id.clone();
        snapshot.earlier_threads = view::earlier_threads(&session);
        snapshot.meta.facts = SessionFacts::from(&session);
        snapshot.meta.model = boot.catalog_model.spec.key.clone();
        snapshot.meta.attachment_limits =
            attachments::Limits::new(boot.catalog_model.spec.max_image_base64_bytes);
        snapshot.meta.context_window_tokens = boot.catalog_model.spec.context_window_tokens;
        snapshot.blocks = blocks;
        snapshot.meta.status = status;
        if let Err(error) = self.start_next(&mut live) {
            live.snapshot.blocks.push(Block::Notice {
                text: error.to_string(),
            });
        }
        self.publish_history(&mut live.snapshot, previous.as_deref());
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn refresh_metadata(&self, session: &ActiveSession) {
        let facts = session.with(|session| SessionFacts::from(session));
        let mut live = self.live.lock().unwrap();
        let current = &live.snapshot.meta.facts;
        if *current == facts {
            return;
        }
        // Session listings show titles and archive state, not usage or modes.
        let listing_changed = current.title != facts.title || current.archived != facts.archived;
        live.snapshot.meta.facts = facts;
        self.publish_metadata(&mut live.snapshot);
        if listing_changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn hosts(&self, status: Vec<myco::HostStatus>) {
        let down: Vec<_> = status
            .into_iter()
            .filter_map(|host| {
                let failure = host.failure?;
                Some(DownHost {
                    name: host.name,
                    since: failure.at,
                    error: failure.message,
                })
            })
            .collect();
        let mut live = self.live.lock().unwrap();
        if live.snapshot.meta.down_hosts != down {
            live.snapshot.meta.down_hosts = down;
            self.publish_metadata(&mut live.snapshot);
        }
    }

    pub(super) fn resources(&self, resources: Vec<myco::core::HostResources>) {
        let mut live = self.live.lock().unwrap();
        let changed = view::refresh_processes(&mut live.snapshot.blocks, &resources);
        if !changed.is_empty() {
            // Catalog subscribers do not consume transcript blocks.
            self.publish_metadata(&mut live.snapshot);
        }
        for index in changed {
            self.publish_block(&mut live.snapshot, index);
        }
    }

    pub(super) fn delta(&self, role: &str, text: String) {
        let mut live = self.live.lock().unwrap();
        let can_append = live
            .generation_start
            .is_none_or(|start| live.snapshot.blocks.len() > start);
        let snapshot = &mut live.snapshot;
        if let Some(Block::Message {
            role: last_role,
            text: last_text,
            ..
        }) = snapshot.blocks.last_mut()
            && last_role == role
            && can_append
        {
            last_text.push_str(&text);
            let index = snapshot.blocks.len() - 1;
            self.publish(
                snapshot,
                json!({"kind":"append", "index":index, "text":text}),
            );
        } else {
            let time = snapshot.blocks.iter().rev().find_map(|block| match block {
                Block::Message { role, time, .. } if role == "user" || role == "system" => {
                    time.clone()
                }
                Block::Boundary { time } => Some(time.clone()),
                _ => None,
            });
            let block = Block::Message {
                role: role.into(),
                text,
                images: vec![],
                time,
            };
            self.push_block(snapshot, block);
        }
    }
}

impl EventSink for App {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::GenerationStarted { context } if context.depth == 0 => {
                let mut live = self.live.lock().unwrap();
                self.acknowledge_events(&mut live);
                live.generation_start = Some(live.snapshot.blocks.len());
                live.service.reset_draft();
                drop(live);
                self.status(Status::Running);
            }
            AgentEvent::GenerationFinished { context } if context.depth == 0 => {
                self.live.lock().unwrap().generation_start = None;
            }
            AgentEvent::GenerationCommitted { context } if context.depth == 0 => {
                let mut live = self.live.lock().unwrap();
                if live.service.commit() {
                    self.cancel_live(&mut live);
                }
            }
            AgentEvent::TextDelta { text, context } if context.depth == 0 => {
                self.live.lock().unwrap().service.text(&text);
                self.delta("assistant", text)
            }
            AgentEvent::ThinkingDelta { text, context } if context.depth == 0 => {
                self.delta("thinking", text)
            }
            AgentEvent::ToolStarted {
                call_id,
                tool_use,
                background,
                context,
            } if context.depth == 0 => {
                let mut live = self.live.lock().unwrap();
                if let Some(heading) = view::assistant_heading(&live.snapshot.blocks) {
                    self.push_block(&mut live.snapshot, heading);
                }
                let can_background = tool_use.name == "bash"
                    && matches!(
                        tool_use.input.get("action").and_then(Value::as_str),
                        None | Some("exec" | "start" | "read" | "write" | "signal")
                    );
                let mut block = Block::tool(tool_use, Some(Instant::now()));
                if let Block::Tool {
                    call_id: id,
                    background_id,
                    ..
                } = &mut block
                {
                    *id = call_id;
                    if can_background {
                        *background_id = Some(call_id);
                        live.background.insert(call_id, background);
                    }
                }
                self.push_block(&mut live.snapshot, block);
                self.preempt_for_events(&mut live);
            }
            AgentEvent::ToolFinished {
                call_id,
                result,
                context,
                ..
            } if context.depth == 0 => {
                let mut live = self.live.lock().unwrap();
                live.background.remove(&call_id);
                let snapshot = &mut live.snapshot;
                if let Some(index) = snapshot.blocks.iter().position(|block| matches!(block, Block::Tool { call_id: id, running: true, .. } if *id == call_id)) {
                    snapshot.blocks[index].finish(&result);
                    snapshot.blocks[index].continue_process();
                    self.publish_block(snapshot, index);
                }
                self.preempt_for_events(&mut live);
            }
            AgentEvent::Failure {
                failure,
                retry_in,
                attempt,
                max_attempts,
                recovery_remaining,
                context,
            } if context.depth == 0 => {
                self.live.lock().unwrap().service.reset_draft();
                let retry = if let Some(delay) = retry_in {
                    self.discard_generation();
                    self.status(Status::Retrying);
                    let budget = recovery_remaining
                        .map(|left| format!(" ({:.1}s recovery budget left)", left.as_secs_f64()))
                        .unwrap_or_default();
                    format!(
                        " — retrying {}/{} in {:.1}s{budget}",
                        attempt + 1,
                        max_attempts,
                        delay.as_secs_f64()
                    )
                } else {
                    String::new()
                };
                self.retry_notice(format!("{}{retry}", failure.cause));
            }
            _ => {}
        }
    }
}
