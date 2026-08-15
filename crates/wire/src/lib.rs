//! The bus envelope over a byte stream — how a protocol provider exports
//! its pool. One JSON object per line (NDJSON), `t`-tagged, in both
//! directions. This crate is the frames and nothing else: no I/O, no
//! tasks — the serve loop (a provider fronting its pool) and the attach
//! loop (a pool adopting a provider's instances) live beside the
//! transport they run on; the frames are what they agree on.
//!
//! The channel contract, which every frame assumes:
//!
//! - **In order and reliable.** The carrier is a byte stream (stdio under
//!   ssh, a pipe, a socket) — frames never reorder and never drop on the
//!   wire. Loss exists only where it always did: a provider's own event
//!   feed is best-effort, and a provider that lags it says so
//!   ([`ToPool::Lagged`]) and re-syncs rows, the same recovery the feed
//!   doctrine demands everywhere else.
//! - **EOF is death.** There is no ping, no timeout, no reconnect frame.
//!   When the stream ends, every instance the provider backed is gone —
//!   removal semantics the pool already has. A new stream is a new
//!   [`ToPool::Hello`] with fresh rows; watchers re-read, as ever. (A
//!   transport that can wedge without closing — a TCP link with
//!   keepalives off — must bring its own liveness; ssh does.)
//! - **The stream is trusted.** Whoever spawned the transport decided the
//!   far end may host instances; authentication happened there, not here.
//!   Principals cross verbatim so attribution and seat law apply where
//!   the cell actually lives — the pool forwards callers, it never
//!   re-judges them.
//!
//! Two versions travel and they govern different things: [`PROTOCOL`] is
//! the shape of these frames — unequal means close the stream, there is
//! nothing to negotiate. A kind's `version` (inside each [`KindOffer`])
//! is that kind's vocabulary, relayed for display and debugging; the pool
//! never interprets a remote kind's spec, it forwards `sys.spec` to the
//! one place that knows.

use serde_json::Value;

pub use myco_instance::{InstanceInfo, Principal, VerbError};
pub use myco_runtime::Watermark;

/// The frame-shape version. Sent in both hellos; unequal ends the stream.
pub const PROTOCOL: u32 = 1;

/// A kind as the provider offers it: name and vocabulary version lifted
/// out for cheap inspection, the full serialized `KindSpec` alongside for
/// surfaces that want the vocabulary without a live instance to ask.
/// Offers are information, not registration — kinds live per-provider,
/// so two hosts both offering `tty` is the normal case, not a collision.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KindOffer {
    pub kind: String,
    pub version: u32,
    pub spec: Value,
}

/// Provider → pool. The provider speaks first.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToPool {
    /// The first frame on the stream: what this provider is, what it can
    /// create, and every instance already alive over there — attach and
    /// resync are the same motion, so a reconnect is just this again.
    Hello {
        protocol: u32,
        /// The provider's self-chosen name (a hostname, a toold's name).
        /// Presentation only; identity is the connection.
        name: String,
        kinds: Vec<KindOffer>,
        rows: Vec<InstanceInfo>,
    },
    /// The answer to a [`ToProvider::Call`] or [`ToProvider::Create`],
    /// matched by `seq`. Replies may interleave with other frames and
    /// with each other — slow verbs do not convoy the stream.
    Reply {
        seq: u64,
        #[serde(flatten)]
        outcome: Outcome,
    },
    /// A listing row, upserted by id: sent for a birth, and again on any
    /// meta change (title, driver, parent). State, not history — the
    /// event that caused it travels separately.
    Row { info: InstanceInfo },
    /// A watermark: coalesced on the provider side like everywhere else;
    /// receipt means "re-read", never "here is what changed".
    Mark { id: String, watermark: Watermark },
    /// One instance event, id-tagged — the provider's global feed,
    /// relayed verbatim.
    Event {
        id: String,
        name: String,
        data: Value,
    },
    /// The instance is no longer over there: removed, or its kind
    /// crashed and the provider dropped it. The pool forgets the row.
    Gone { id: String },
    /// The provider lagged its own event feed: frames were lost before
    /// the wire. Fresh [`ToPool::Row`]s follow for every live instance;
    /// consumers re-read, which is what they do on lag anyway.
    Lagged,
}

/// Pool → provider.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToProvider {
    /// The pool's answer to the provider's hello. A pool that sees an
    /// unequal `protocol` closes instead of sending this.
    Hello { protocol: u32 },
    /// Dispatch one verb — `sys.*` included: the seat, the meta, the
    /// removal all live where the cell lives, and the pool forwards
    /// rather than re-implements.
    Call {
        seq: u64,
        id: String,
        caller: Principal,
        verb: String,
        args: Value,
    },
    /// Create an instance over there. The one operation that is not a
    /// verb, mirroring the pool's own API. The reply's `ok` is the new
    /// row (`InstanceInfo`).
    Create {
        seq: u64,
        kind: String,
        project: String,
        title: String,
        creator: Principal,
        args: Value,
    },
}

/// A reply's payload: the verb result or the verb error, exactly one.
/// Untagged with `Err` first, so the discriminator is the field name —
/// an `ok` payload that happens to contain an `err` key nests under
/// `"ok"` and cannot confuse the match.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Outcome {
    Err { err: VerbError },
    Ok { ok: Value },
}

impl Outcome {
    pub fn into_result(self) -> Result<Value, VerbError> {
        match self {
            Outcome::Ok { ok } => Ok(ok),
            Outcome::Err { err } => Err(err),
        }
    }
}

impl From<Result<Value, VerbError>> for Outcome {
    fn from(r: Result<Value, VerbError>) -> Self {
        match r {
            Ok(ok) => Outcome::Ok { ok },
            Err(err) => Outcome::Err { err },
        }
    }
}

/// One frame, one line: the serialized object and the terminating
/// newline. Frames never contain raw newlines (JSON strings escape
/// them), so lines are a safe framing.
pub fn encode<T: serde::Serialize>(frame: &T) -> String {
    let mut line = serde_json::to_string(frame).expect("frames serialize");
    line.push('\n');
    line
}

/// Parse one line (with or without its newline) as a frame. A frame that
/// does not parse is a protocol error worth closing the stream over —
/// the two ends do not negotiate, they agree or hang up.
pub fn decode<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip<T>(frame: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        decode(&encode(frame)).expect("roundtrips")
    }

    /// The wire shapes are pinned exactly: these strings are the
    /// protocol, and changing one is a [`PROTOCOL`] bump, not a refactor.
    #[test]
    fn golden_frames_to_pool() {
        let hello = ToPool::Hello {
            protocol: 1,
            name: "buildbox".into(),
            kinds: vec![KindOffer {
                kind: "tty".into(),
                version: 1,
                spec: json!({"kind": "tty"}),
            }],
            rows: vec![],
        };
        assert_eq!(
            encode(&hello),
            "{\"t\":\"hello\",\"protocol\":1,\"name\":\"buildbox\",\"kinds\":[{\"kind\":\"tty\",\"version\":1,\"spec\":{\"kind\":\"tty\"}}],\"rows\":[]}\n"
        );
        assert_eq!(
            encode(&ToPool::Mark {
                id: "i-1".into(),
                watermark: 7
            }),
            "{\"t\":\"mark\",\"id\":\"i-1\",\"watermark\":7}\n"
        );
        assert_eq!(
            encode(&ToPool::Event {
                id: "i-1".into(),
                name: "exited".into(),
                data: json!({"code": 0}),
            }),
            "{\"t\":\"event\",\"id\":\"i-1\",\"name\":\"exited\",\"data\":{\"code\":0}}\n"
        );
        assert_eq!(
            encode(&ToPool::Gone { id: "i-1".into() }),
            "{\"t\":\"gone\",\"id\":\"i-1\"}\n"
        );
        assert_eq!(encode(&ToPool::Lagged), "{\"t\":\"lagged\"}\n");
    }

    #[test]
    fn golden_frames_to_provider() {
        assert_eq!(
            encode(&ToProvider::Hello { protocol: 1 }),
            "{\"t\":\"hello\",\"protocol\":1}\n"
        );
        let call = ToProvider::Call {
            seq: 3,
            id: "i-1".into(),
            caller: Principal::Human("ada".into()),
            verb: "input".into(),
            args: json!({"data": "ls\r"}),
        };
        assert_eq!(
            encode(&call),
            "{\"t\":\"call\",\"seq\":3,\"id\":\"i-1\",\"caller\":{\"kind\":\"human\",\"id\":\"ada\"},\"verb\":\"input\",\"args\":{\"data\":\"ls\\r\"}}\n"
        );
        let create = ToProvider::Create {
            seq: 4,
            kind: "tty".into(),
            project: "default".into(),
            title: "build shell".into(),
            creator: Principal::Human("ada".into()),
            args: json!({}),
        };
        assert_eq!(
            encode(&create),
            "{\"t\":\"create\",\"seq\":4,\"kind\":\"tty\",\"project\":\"default\",\"title\":\"build shell\",\"creator\":{\"kind\":\"human\",\"id\":\"ada\"},\"args\":{}}\n"
        );
    }

    /// The reply discriminates on field name, `err` tried first — and an
    /// `ok` payload containing an `err` key stays an ok, because it
    /// nests under `"ok"` rather than flattening into the frame.
    #[test]
    fn replies_carry_exactly_ok_or_err() {
        let ok = ToPool::Reply {
            seq: 3,
            outcome: Ok(json!({"rows": 2})).into(),
        };
        assert_eq!(
            encode(&ok),
            "{\"t\":\"reply\",\"seq\":3,\"ok\":{\"rows\":2}}\n"
        );

        let err = ToPool::Reply {
            seq: 4,
            outcome: Err(VerbError::NotDriver { held_by: None }).into(),
        };
        assert_eq!(
            encode(&err),
            "{\"t\":\"reply\",\"seq\":4,\"err\":{\"error\":\"not_driver\",\"held_by\":null}}\n"
        );

        let ToPool::Reply { outcome, .. } = roundtrip(&err) else {
            panic!("reply survives the wire");
        };
        assert_eq!(
            outcome.into_result(),
            Err(VerbError::NotDriver { held_by: None })
        );

        let tricky = ToPool::Reply {
            seq: 5,
            outcome: Ok(json!({"err": "just data"})).into(),
        };
        let ToPool::Reply { outcome, .. } = roundtrip(&tricky) else {
            panic!("reply survives the wire");
        };
        assert_eq!(outcome.into_result(), Ok(json!({"err": "just data"})));

        // A null result is still an ok — the key is the discriminator,
        // not the payload.
        let null = ToPool::Reply {
            seq: 6,
            outcome: Ok(Value::Null).into(),
        };
        assert_eq!(encode(&null), "{\"t\":\"reply\",\"seq\":6,\"ok\":null}\n");
        let ToPool::Reply { outcome, .. } = roundtrip(&null) else {
            panic!("reply survives the wire");
        };
        assert_eq!(outcome.into_result(), Ok(Value::Null));
    }

    /// Rows ride the hello so attach and resync are one motion; the row
    /// is the same `InstanceInfo` every listing speaks.
    #[test]
    fn hello_rows_are_listing_rows() {
        let row: InstanceInfo = serde_json::from_value(json!({
            "id": "i-1",
            "kind": "tty",
            "project": "default",
            "title": "shell",
            "creator": {"kind": "human", "id": "ada"},
            "parent": null,
            "driver": {"kind": "human", "id": "ada"},
            "watermark": 12,
            "crashed": false,
            "created_at": "2026-08-08T00:00:00Z",
        }))
        .expect("a listing row deserializes");
        let hello = ToPool::Hello {
            protocol: PROTOCOL,
            name: "buildbox".into(),
            kinds: vec![],
            rows: vec![row.clone()],
        };
        let ToPool::Hello { rows, .. } = roundtrip(&hello) else {
            panic!("hello survives the wire");
        };
        assert_eq!(rows, vec![row]);
    }

    /// Trailing newlines and their absence both parse — the decoder does
    /// not care how the line reader split.
    #[test]
    fn decode_tolerates_line_endings() {
        let bare = "{\"t\":\"lagged\"}";
        assert_eq!(decode::<ToPool>(bare).unwrap(), ToPool::Lagged);
        assert_eq!(
            decode::<ToPool>(&format!("{bare}\r\n")).unwrap(),
            ToPool::Lagged
        );
    }
}
