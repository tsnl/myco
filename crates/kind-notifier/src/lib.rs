//! The `notifier` kind: one instance per human, their attention inbox —
//! DESIGN.md's worked requirement, built as specified: **a kind, not a
//! feature**. L2 find-or-creates one per person at sign-in, *as that
//! person*, so `owner_only` (the creator axis) is the entire privacy
//! story: attention is private, and the framework already enforces it.
//!
//! **Ingest** is a side-feed consuming the pool's global event feed — the
//! first side-feed whose input outlives its instance, hence the
//! Drop-cancel. It turns [`Attention`] events addressed to the owner into
//! items, writing through the framework's [`Shared`] so every landing
//! wakes the watchers; the watermark is not the badge (acks bump too) —
//! the `unacked` count in the payload is.
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

use chrono::{DateTime, Utc};
use myco_instance::events::{ATTENTION, Attention};
use myco_instance::{
    CreateCtx, Instance, Kind, KindSpec, Pool, Principal, Shared, VerbError, VerbSpec,
};
use myco_runtime::Signals;
use serde_json::{Value, json};

pub mod push;

pub use push::Pusher;

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
            "store a web-push {subscription}; write-only state, delivery on live items",
        ),
        VerbSpec::owned_read(
            "push_key",
            "the VAPID public {key} a browser must subscribe with",
        ),
        VerbSpec::owned(
            "unregister",
            "forget the web-push subscription with {endpoint}",
        ),
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
    pusher: Option<std::sync::Arc<Pusher>>,
}

impl NotifierKind {
    pub fn new(pool: Pool) -> Self {
        Self { pool, pusher: None }
    }

    /// A kind that can also *wake* its owners: live items go out through
    /// the pusher to every registered endpoint. Without one, register
    /// still stores — delivery is the only thing missing.
    pub fn with_push(pool: Pool, pusher: std::sync::Arc<Pusher>) -> Self {
        Self {
            pool,
            pusher: Some(pusher),
        }
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
        let inbox = Shared::new(Inbox::default(), signals);
        let ingest = tokio::spawn(ingest(
            self.pool.clone(),
            ctx.creator.clone(),
            inbox.clone(),
            feed,
            self.pusher.clone(),
        ));
        Ok(Box::new(Notifier {
            owner: ctx.creator.clone(),
            pool: self.pool.clone(),
            inbox,
            ingest,
            pusher: self.pusher.clone(),
        }))
    }
}

struct Notifier {
    owner: Principal,
    pool: Pool,
    inbox: Shared<Inbox>,
    ingest: tokio::task::JoinHandle<()>,
    pusher: Option<std::sync::Arc<Pusher>>,
}

impl Drop for Notifier {
    /// The side-feed rule: the global event feed outlives this instance,
    /// so the task consuming it must not.
    fn drop(&mut self) {
        self.ingest.abort();
    }
}

/// The reducer over the feed: birth reconcile, then events; on lag,
/// reconcile again. The side-feed writes through the [`Shared`], so every
/// item that lands wakes the badge's watchers without a bump written here.
async fn ingest(
    pool: Pool,
    owner: Principal,
    inbox: Shared<Inbox>,
    mut feed: tokio::sync::broadcast::Receiver<(String, myco_runtime::Event)>,
    pusher: Option<std::sync::Arc<Pusher>>,
) {
    reconcile(&pool, &owner, &inbox).await;
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
                let added = inbox.with(|i| i.add(&source, &attention));
                // Push wakes for the live moment only — reconcile's
                // catch-up never pushes, so a booting notifier cannot
                // replay history onto a phone. The inbox is the record;
                // a push that misses costs a wake, not an item.
                if added && let Some(pusher) = &pusher {
                    deliver(pusher.clone(), inbox.clone(), &attention);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                reconcile(&pool, &owner, &inbox).await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Fan one live item out to every registered endpoint, off-loop. A 404
/// or 410 prunes the endpoint (the push service said it is dead); any
/// other failure is logged by nobody — best-effort is the contract.
fn deliver(
    pusher: std::sync::Arc<Pusher>,
    inbox: Shared<Inbox>,
    attention: &myco_instance::events::Attention,
) {
    let endpoints = inbox.read(|i| i.endpoints.clone());
    if endpoints.is_empty() {
        return;
    }
    let payload = json!({ "title": attention.title, "body": attention.body }).to_string();
    tokio::spawn(async move {
        for subscription in endpoints {
            if let Ok(404 | 410) = pusher.send(&subscription, payload.as_bytes()).await {
                let Some(dead) = subscription
                    .get("endpoint")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    continue;
                };
                inbox.with(|i| {
                    i.endpoints.retain(|s| {
                        s.get("endpoint").and_then(Value::as_str) != Some(dead.as_str())
                    })
                });
            }
        }
    });
}

/// Re-derive items from the re-readable record: for every chat whose
/// watermark moved past our cursor, re-read its transcript from the
/// cursor and itemize mentions of the owner. Idempotent by construction
/// (the (source, seq) dedupe), so racing the live feed is harmless.
async fn reconcile(pool: &Pool, owner: &Principal, inbox: &Shared<Inbox>) {
    let Principal::Human(owner_id) = owner else {
        return;
    };
    for info in pool.list(None) {
        if info.kind != "chat" {
            continue;
        }
        let cursor = inbox.read(|i| i.cursors.get(&info.id).copied().unwrap_or(0));
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
        inbox.with(|i| {
            for entry in page["entries"].as_array().into_iter().flatten() {
                let (Some(text), Some(seq)) = (entry["text"].as_str(), entry["seq"].as_u64())
                else {
                    continue;
                };
                if !text
                    .to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
                {
                    continue;
                }
                let author = entry["author"]["id"].as_str().unwrap_or("someone");
                let attention = Attention {
                    for_: vec![owner.clone()],
                    title: format!("mentioned by {author}"),
                    body: text.chars().take(120).collect(),
                    seq: Some(seq),
                };
                i.add(&info.id, &attention);
            }
            i.cursors.insert(info.id.clone(), next);
        });
    }
}

#[async_trait::async_trait]
impl Instance for Notifier {
    async fn verb(
        &mut self,
        _caller: &Principal,
        verb: &str,
        args: Value,
        _signals: &Signals,
    ) -> Result<Value, VerbError> {
        // owner_only is enforced by the framework before we run; `_caller`
        // here is always the owner.
        match verb {
            "pending" => Ok(self.inbox.read(|i| {
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
                json!({
                    "items": items,
                    "next": upto,
                    "unacked": i.unacked(),
                    "len": len,
                })
            })),
            "text" => Ok(self.inbox.read(|i| {
                let mut out = String::new();
                for item in i.items.iter().filter(|it| it.seq >= i.acked_upto) {
                    out.push_str(&format!("{} — {}\n", item.title, item.body));
                }
                json!({ "text": out, "unacked": i.unacked() })
            })),
            "ack" => {
                let upto =
                    args.get("upto")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| VerbError::BadArgs {
                            why: "ack needs {upto}: the highest seq you have seen".into(),
                        })?;
                // Acks are observable: the badge is the payload's
                // `unacked`, and its watchers re-read to see it fall.
                Ok(self.inbox.with(|i| {
                    let target = (upto + 1).min(i.items.len() as u64);
                    i.acked_upto = i.acked_upto.max(target);
                    json!({ "unacked": i.unacked() })
                }))
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
                Ok(self.inbox.with(|i| {
                    if on {
                        i.muted.insert(source.to_string());
                    } else {
                        i.muted.remove(source);
                    }
                    json!({ "muted": i.muted })
                }))
            }
            "push_key" => match &self.pusher {
                Some(pusher) => Ok(json!({ "key": pusher.public_key_b64() })),
                None => Err(VerbError::Denied {
                    why: "web-push is not configured on this server".into(),
                }),
            },
            "register" => {
                let sub = args
                    .get("subscription")
                    .cloned()
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "register needs {subscription}: the browser's PushSubscription JSON"
                            .into(),
                    })?;
                let endpoint = sub
                    .get("endpoint")
                    .and_then(Value::as_str)
                    .map(String::from);
                Ok(self.inbox.with(|i| {
                    if let Some(e) = &endpoint {
                        i.endpoints
                            .retain(|s| s.get("endpoint").and_then(Value::as_str) != Some(e));
                    }
                    i.endpoints.push(sub);
                    json!({ "registered": true })
                }))
            }
            "unregister" => {
                let endpoint = args
                    .get("endpoint")
                    .and_then(Value::as_str)
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "unregister needs {endpoint}".into(),
                    })?;
                Ok(self.inbox.with(|i| {
                    let before = i.endpoints.len();
                    i.endpoints
                        .retain(|s| s.get("endpoint").and_then(Value::as_str) != Some(endpoint));
                    json!({ "dropped": before != i.endpoints.len() })
                }))
            }
            "reconcile" => {
                reconcile(&self.pool, &self.owner, &self.inbox).await;
                Ok(self
                    .inbox
                    .read(|i| json!({ "unacked": i.unacked(), "len": i.items.len() as u64 })))
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

#[cfg(test)]
mod tests;
