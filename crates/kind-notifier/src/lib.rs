//! The `notifier` kind: one instance per human, their attention inbox —
//! DESIGN.md's worked requirement, built as specified: **a kind, not a
//! feature**. L2 find-or-creates one per person at sign-in, *as that
//! person*, so `owner_only` (the creator axis) is the entire privacy
//! story: attention is private, and the framework already enforces it.
//!
//! **Ingest** is a side-feed consuming the pool's global event feed — the
//! first side-feed whose input outlives its instance, hence the
//! Drop-cancel. It turns [`Attention`] events addressed to the owner into
//! items and bumps; the watermark is not the badge (acks bump too) — the
//! `unacked` count in the payload is.
//!
//! **Loss:** the feed is a hint, not the ledger. On `Lagged` — and once at
//! birth — the notifier reconciles: list the pool, re-read the cursored
//! reads of whatever moved past its per-source cursors, and re-derive
//! items. A mention must not vanish because a chat burst-emitted, and it
//! cannot, because mentions are re-readable in the transcript. Birth
//! reconcile also means a mention posted before your first login still
//! reaches you.
//!
//! Web-push **delivery** is M4; `register`/`unregister` already store the
//! endpoints as write-only state no read returns.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use myco_instance::events::{ATTENTION, Attention};
use myco_instance::{
    CreateCtx, Instance, Kind, KindSpec, Pool, Principal, VerbError, VerbSpec,
};
use myco_runtime::Signals;
use serde_json::{Value, json};

static NOTIFIER_SPEC: KindSpec = KindSpec {
    kind: "notifier",
    version: 1,
    doc: "a person's attention inbox: mentions and other wants-you moments, private to its owner",
    verbs: &[
        VerbSpec::owned_cursored_read(
            "pending",
            "attention items from sequence {from}, at most {max_entries}; returns {next}, \
             {unacked}, {len}",
        ),
        VerbSpec::owned_read(
            "text",
            "the unacked items as plain text — the agent's window into what its human has \
             not seen",
        ),
        VerbSpec::owned("ack", "mark items through sequence {upto} as seen"),
        VerbSpec::owned(
            "mute",
            "{source, on?}: stop (or resume, on=false) itemizing attention from an instance",
        ),
        VerbSpec::owned(
            "register",
            "store a web-push {subscription} (delivery lands in M4); write-only state",
        ),
        VerbSpec::owned("unregister", "forget the web-push subscription with {endpoint}"),
        VerbSpec::owned(
            "reconcile",
            "re-derive items from the re-readable record (also runs on feed lag and at birth)",
        ),
    ],
    primary_render: "pending",
    recommended_context: "text",
};

const DEFAULT_PAGE: u64 = 100;

/// One attention item. `seq` is notifier-local and dense; `source_seq` is
/// the emitting instance's cursor for the moment (dedupe + reconcile key).
#[derive(Debug, Clone, serde::Serialize)]
struct Item {
    seq: u64,
    at: DateTime<Utc>,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_seq: Option<u64>,
    title: String,
    body: String,
}

#[derive(Default)]
struct Inbox {
    items: Vec<Item>,
    /// Items with `seq < acked_upto` are seen. Dense seqs make the unacked
    /// count arithmetic.
    acked_upto: u64,
    muted: BTreeSet<String>,
    /// Web-push subscriptions, opaque. Write-only: no read returns them.
    endpoints: Vec<Value>,
    /// Per-source cursors: how far reconcile has scanned each instance.
    cursors: HashMap<String, u64>,
}

impl Inbox {
    fn unacked(&self) -> u64 {
        self.items.len() as u64 - self.acked_upto.min(self.items.len() as u64)
    }

    /// Add unless muted or already present (same source + source_seq).
    /// Returns whether anything changed.
    fn add(&mut self, source: &str, attention: &Attention) -> bool {
        if self.muted.contains(source) {
            return false;
        }
        if let Some(seq) = attention.seq
            && self
                .items
                .iter()
                .any(|i| i.source == source && i.source_seq == Some(seq))
        {
            return false;
        }
        self.items.push(Item {
            seq: self.items.len() as u64,
            at: Utc::now(),
            source: source.to_string(),
            source_seq: attention.seq,
            title: attention.title.clone(),
            body: attention.body.clone(),
        });
        true
    }
}

pub struct NotifierKind {
    pool: Pool,
}

impl NotifierKind {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl Kind for NotifierKind {
    fn spec(&self) -> &'static KindSpec {
        &NOTIFIER_SPEC
    }

    fn create(
        &self,
        ctx: &CreateCtx,
        _args: Value,
        signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        // Subscribe before the task spawns: events during startup wait in
        // the channel rather than being missed.
        let feed = self.pool.events();
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let ingest = tokio::spawn(ingest(
            self.pool.clone(),
            ctx.creator.clone(),
            Arc::clone(&inbox),
            feed,
            signals,
        ));
        Ok(Box::new(Notifier {
            owner: ctx.creator.clone(),
            pool: self.pool.clone(),
            inbox,
            ingest,
        }))
    }
}

struct Notifier {
    owner: Principal,
    pool: Pool,
    inbox: Arc<Mutex<Inbox>>,
    ingest: tokio::task::JoinHandle<()>,
}

impl Drop for Notifier {
    /// The side-feed rule: the global event feed outlives this instance,
    /// so the task consuming it must not.
    fn drop(&mut self) {
        self.ingest.abort();
    }
}

/// The reducer over the feed: birth reconcile, then events; on lag,
/// reconcile again. Bumps only when something changed.
async fn ingest(
    pool: Pool,
    owner: Principal,
    inbox: Arc<Mutex<Inbox>>,
    mut feed: tokio::sync::broadcast::Receiver<(String, myco_runtime::Event)>,
    signals: Signals,
) {
    if reconcile(&pool, &owner, &inbox).await {
        signals.bump();
    }
    loop {
        match feed.recv().await {
            Ok((source, event)) => {
                if event.name != ATTENTION {
                    continue;
                }
                let Some(attention) = Attention::from_data(&event.data) else {
                    continue;
                };
                if !attention.for_.contains(&owner) {
                    continue;
                }
                let added = {
                    let mut i = inbox.lock().unwrap_or_else(|e| e.into_inner());
                    i.add(&source, &attention)
                };
                if added {
                    signals.bump();
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                if reconcile(&pool, &owner, &inbox).await {
                    signals.bump();
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Re-derive items from the re-readable record: for every chat whose
/// watermark moved past our cursor, re-read its transcript from the
/// cursor and itemize mentions of the owner. Idempotent by construction
/// (the (source, seq) dedupe), so racing the live feed is harmless.
async fn reconcile(pool: &Pool, owner: &Principal, inbox: &Arc<Mutex<Inbox>>) -> bool {
    let Principal::Human(owner_id) = owner else {
        return false;
    };
    let mut changed = false;
    for info in pool.list(None) {
        if info.kind != "chat" {
            continue;
        }
        let cursor = {
            let i = inbox.lock().unwrap_or_else(|e| e.into_inner());
            i.cursors.get(&info.id).copied().unwrap_or(0)
        };
        let Ok(page) = pool
            .call(
                owner,
                &info.id,
                "tail",
                json!({"from": cursor, "max_entries": 10_000}),
            )
            .await
        else {
            continue;
        };
        let next = page["next"].as_u64().unwrap_or(cursor);
        let needle = format!("@{owner_id}");
        let mut i = inbox.lock().unwrap_or_else(|e| e.into_inner());
        for entry in page["entries"].as_array().into_iter().flatten() {
            let (Some(text), Some(seq)) = (entry["text"].as_str(), entry["seq"].as_u64()) else {
                continue;
            };
            if !text.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()) {
                continue;
            }
            let author = entry["author"]["id"].as_str().unwrap_or("someone");
            let attention = Attention {
                for_: vec![owner.clone()],
                title: format!("mentioned by {author}"),
                body: text.chars().take(120).collect(),
                seq: Some(seq),
            };
            changed |= i.add(&info.id, &attention);
        }
        i.cursors.insert(info.id.clone(), next);
    }
    changed
}

#[async_trait::async_trait]
impl Instance for Notifier {
    async fn verb(
        &mut self,
        _caller: &Principal,
        verb: &str,
        args: Value,
        signals: &Signals,
    ) -> Result<Value, VerbError> {
        // owner_only is enforced by the framework before we run; `_caller`
        // here is always the owner.
        match verb {
            "pending" => {
                let i = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                let len = i.items.len() as u64;
                let from = args
                    .get("from")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(len);
                let max = args
                    .get("max_entries")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_PAGE);
                let upto = len.min(from.saturating_add(max));
                let items: Vec<Value> = i.items[from as usize..upto as usize]
                    .iter()
                    .map(|item| {
                        let mut v = serde_json::to_value(item).expect("item serializes");
                        v["acked"] = json!(item.seq < i.acked_upto);
                        v
                    })
                    .collect();
                Ok(json!({
                    "items": items,
                    "next": upto,
                    "unacked": i.unacked(),
                    "len": len,
                }))
            }
            "text" => {
                let i = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                let mut out = String::new();
                for item in i.items.iter().filter(|it| it.seq >= i.acked_upto) {
                    out.push_str(&format!("{} — {}\n", item.title, item.body));
                }
                Ok(json!({ "text": out, "unacked": i.unacked() }))
            }
            "ack" => {
                let upto = args.get("upto").and_then(Value::as_u64).ok_or_else(|| {
                    VerbError::BadArgs {
                        why: "ack needs {upto}: the highest seq you have seen".into(),
                    }
                })?;
                let changed = {
                    let mut i = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                    let target = (upto + 1).min(i.items.len() as u64);
                    let changed = target > i.acked_upto;
                    i.acked_upto = i.acked_upto.max(target);
                    changed
                };
                if changed {
                    // Acks bump too: the badge is the payload's `unacked`,
                    // and its watchers need to re-read to see it fall.
                    signals.bump();
                }
                let i = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                Ok(json!({ "unacked": i.unacked() }))
            }
            "mute" => {
                let source = args
                    .get("source")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "mute needs {source}: an instance id".into(),
                    })?;
                let on = args.get("on").and_then(Value::as_bool).unwrap_or(true);
                let mut i = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                if on {
                    i.muted.insert(source.to_string());
                } else {
                    i.muted.remove(source);
                }
                signals.bump();
                Ok(json!({ "muted": i.muted }))
            }
            "register" => {
                let sub = args.get("subscription").cloned().ok_or_else(|| {
                    VerbError::BadArgs {
                        why: "register needs {subscription}: the browser's PushSubscription JSON"
                            .into(),
                    }
                })?;
                let endpoint = sub.get("endpoint").and_then(Value::as_str).map(String::from);
                let mut i = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(e) = &endpoint {
                    i.endpoints
                        .retain(|s| s.get("endpoint").and_then(Value::as_str) != Some(e));
                }
                i.endpoints.push(sub);
                Ok(json!({ "registered": true }))
            }
            "unregister" => {
                let endpoint = args
                    .get("endpoint")
                    .and_then(Value::as_str)
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "unregister needs {endpoint}".into(),
                    })?;
                let mut i = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                let before = i.endpoints.len();
                i.endpoints
                    .retain(|s| s.get("endpoint").and_then(Value::as_str) != Some(endpoint));
                Ok(json!({ "dropped": before != i.endpoints.len() }))
            }
            "reconcile" => {
                let changed = reconcile(&self.pool, &self.owner, &self.inbox).await;
                if changed {
                    signals.bump();
                }
                let i = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                Ok(json!({ "unacked": i.unacked(), "len": i.items.len() as u64 }))
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

#[cfg(test)]
mod tests;
