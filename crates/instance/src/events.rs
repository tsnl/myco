//! Pinned cross-kind event conventions — the counterpart of `sys.*` for
//! things kinds *emit* rather than answer. Pinned here for the same reason:
//! a convention every kind spells its own way is how one concept becomes N
//! shapes.
//!
//! The runtime's [`Event`](myco_runtime::Event) stays `{name, data}` on
//! purpose: L0 knows nothing of principals, and the broadcast does no
//! routing, so an addressee field would move a string without changing any
//! mechanics. Addressing is a payload convention, typed once, here.

use crate::Principal;
use serde_json::Value;

/// Event name for "a principal's attention is wanted": a chat turn ended
/// waiting on input, a mention landed, a long job finished. Consumed by
/// per-user notifier instances and by an agent's standing subscriptions —
/// wake-the-human and wake-the-agent are the same shape.
pub const ATTENTION: &str = "attention";

/// The `attention` event's payload. Emitters must also keep the underlying
/// moment re-readable behind a cursored read — the feed is best-effort, so
/// a lagging consumer recovers by re-reading, and an event nothing can
/// re-read is an event that can be silently lost.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attention {
    /// Whose attention. Empty means "anyone watching".
    #[serde(rename = "for", default)]
    pub for_: Vec<Principal>,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// The emitting instance's cursor position for the underlying moment
    /// (a chat entry's seq), when there is one — consumers dedupe on
    /// (source, seq) and reconcile by re-reading from a cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

impl Attention {
    pub fn data(&self) -> Value {
        serde_json::to_value(self).expect("attention serializes")
    }

    pub fn from_data(data: &Value) -> Option<Self> {
        serde_json::from_value(data.clone()).ok()
    }
}
