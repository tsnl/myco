//! The conversation's state machine: one struct, one `apply`, every event a
//! typed action.
//!
//! Everything the live view knows — transcript, stream buffer, busy flag,
//! error line, room-ness — mutates in exactly one place, against the *current*
//! state. The long-lived event loops (the SSE reader, the slow poll, the send
//! path) hold only a dispatcher; they never read state they captured at spawn
//! time. That shape is the fix for a whole family of bugs: a Yew
//! `UseStateHandle` dereferences to the value from the render that created
//! it, so a task spawned once kept "merging" arrivals into the transcript as
//! it looked at its first render — empty — and every incoming message wiped
//! the screen down to itself until the next poll happened to restore it.
//!
//! Deliberately yew-free so the whole machine is unit-testable on the host:
//! the component owns the `Reducible` shim.

use myco_api as api;

/// One piece of the turn currently arriving.
///
/// The streaming buffer used to be a single `String`, which forced every kind
/// of live event through the same channel — a tool start had to be written
/// into the prose as `● bash` text, because a string cannot hold a widget.
/// Modelling the buffer as a list of typed items instead lets a tool call
/// stream as the same card it becomes once the turn is saved.
#[derive(Clone, PartialEq)]
pub enum StreamItem {
    Text(String),
    Thinking(String),
    /// A call in flight — or finished, once its result arrives. `id` is the
    /// provider's id for the call: the same identity the saved entry carries,
    /// so the streaming card and the persisted card are one element to the
    /// renderer, not a removal and an insertion.
    Tool {
        id: String,
        name: String,
        input: serde_json::Value,
        result: Option<api::ToolResult>,
    },
}

impl StreamItem {
    /// Roughly how much has arrived; the scroll effect watches this so growing
    /// text keeps the pane pinned to the bottom, not just new items.
    pub fn size(&self) -> usize {
        match self {
            StreamItem::Text(t) | StreamItem::Thinking(t) => t.len(),
            StreamItem::Tool { name, result, .. } => name.len() + result.as_ref().map_or(0, |_| 1),
        }
    }
}

/// Everything the conversation view renders from.
#[derive(Clone)]
pub struct ConvState {
    /// Who is looking; fixes which messages read as "mine".
    pub me: Option<api::Identity>,
    pub entries: Vec<api::Entry>,
    /// The turn streaming right now, cleared when its persisted form arrives.
    pub streaming: Vec<StreamItem>,
    /// Messages delivered while that turn streams. They arrived *during* the
    /// turn, so they render after its buffer — appending them to `entries`
    /// would show them above output that started before they were sent, and
    /// they would visibly jump below it when the turn persisted. Folded into
    /// `entries` at the next boundary.
    pub arrivals: Vec<api::Entry>,
    pub busy: bool,
    pub error: Option<String>,
    /// Whether anyone but us has posted here; drives the addressing hint.
    pub shared: bool,
    /// The session's catalog model key, for the topbar picker. Empty until
    /// the first detail fetch reports it.
    pub model: String,
    /// Per-session effort override; `None` renders as the model's default.
    pub effort: Option<String>,
}

impl ConvState {
    pub fn new(me: Option<api::Identity>) -> Self {
        Self {
            me,
            entries: Vec::new(),
            streaming: Vec::new(),
            arrivals: Vec::new(),
            busy: false,
            error: None,
            shared: false,
            model: String::new(),
            effort: None,
        }
    }

    fn mine(&self, author: &api::Author) -> bool {
        matches!((author, &self.me),
            (api::Author::User { id, .. }, Some(i)) if *id == i.id)
    }
}

/// One thing that happened, from whichever loop saw it.
pub enum ConvAction {
    /// The opening transcript fetch — also re-dispatched when the event
    /// stream reconnects, so a missed window heals.
    Loaded {
        entries: Vec<api::Entry>,
        busy: bool,
    },
    /// Somebody posted; the feed delivered the entry itself.
    Message {
        entry: api::Entry,
        wakes_agent: bool,
    },
    TurnStarted,
    Text(String),
    Thinking(String),
    ToolStarted {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The matching result for a streaming card: complete it in place.
    ToolFinished {
        result: api::ToolResult,
    },
    TurnFailed(String),
    /// The refetch that follows the wire's `TurnFinished`: the server has
    /// persisted the turn, so the buffer and the transcript swap in one step
    /// — the streamed content never disappears before its saved form shows.
    TurnEnded {
        poll: api::Poll,
    },
    /// The slow reconciliation poll. Adopted only while no stream buffer is
    /// held: mid-turn, the checkpointed entries cover output the buffer still
    /// shows, and adopting them would render the same prose and the same
    /// card twice.
    Polled {
        poll: api::Poll,
    },
    /// What the server said when we posted: whether a reply is coming or
    /// already underway.
    Sent {
        busy: bool,
    },
    /// The session's model/effort as the server reports it — from the detail
    /// fetch on (re)connect, or the response to our own `PATCH`.
    Configured {
        model: String,
        effort: Option<String>,
    },
    Failed(String),
}

impl ConvState {
    pub fn apply(&mut self, action: ConvAction) {
        match action {
            ConvAction::Loaded { entries, busy } => {
                self.shared = is_shared(&entries, self.me.as_ref());
                self.entries = entries;
                self.busy = busy;
                // A full reload is the whole truth — dispatched on (re)connect,
                // where a stale buffer would double whatever it still holds.
                self.streaming.clear();
                self.arrivals.clear();
            }
            ConvAction::Message { entry, wakes_agent } => {
                if !self.mine(&entry.author) {
                    self.shared = true;
                }
                // Mid-turn deliveries hold position *after* the stream buffer
                // (that is where the room saw them); otherwise straight in.
                if self.busy || !self.streaming.is_empty() {
                    if !self.entries.iter().any(|e| e.at == entry.at) {
                        merge(&mut self.arrivals, entry);
                    }
                } else {
                    merge(&mut self.entries, entry);
                }
                if wakes_agent {
                    self.busy = true;
                }
            }
            ConvAction::TurnStarted => {
                self.busy = true;
                self.error = None;
                self.streaming.clear();
                // The boundary passed: what arrived mid-turn is part of the
                // transcript now (the refetch that follows orders it).
                for entry in std::mem::take(&mut self.arrivals) {
                    merge(&mut self.entries, entry);
                }
            }
            ConvAction::Text(t) => push_text(&mut self.streaming, t, false),
            ConvAction::Thinking(t) => push_text(&mut self.streaming, t, true),
            ConvAction::ToolStarted { id, name, input } => {
                self.streaming.push(StreamItem::Tool {
                    id,
                    name,
                    input,
                    result: None,
                });
            }
            ConvAction::ToolFinished { result } => {
                // Complete the card it answers; an unmatched result (the
                // stream connected mid-turn) has no card to complete.
                let call_id = result.id.clone();
                if let Some(StreamItem::Tool { result: slot, .. }) =
                    self.streaming.iter_mut().rev().find(|item| {
                        matches!(item, StreamItem::Tool { id, result: None, .. } if *id == call_id)
                    })
                {
                    *slot = Some(result);
                }
            }
            ConvAction::TurnFailed(message) => self.error = Some(message),
            ConvAction::TurnEnded { poll } => {
                for entry in std::mem::take(&mut self.arrivals) {
                    merge(&mut self.entries, entry);
                }
                reconcile(&mut self.entries, poll.entries);
                self.streaming.clear();
                self.busy = poll.busy;
                if poll.last_error.is_some() {
                    self.error = poll.last_error;
                }
            }
            ConvAction::Polled { poll } => {
                if self.streaming.is_empty() {
                    reconcile(&mut self.entries, poll.entries);
                    // An arrival the server has absorbed is transcript now —
                    // holding it would render it twice.
                    let entries = &self.entries;
                    self.arrivals
                        .retain(|a| !entries.iter().any(|e| e.at == a.at));
                }
                self.busy = poll.busy;
            }
            ConvAction::Sent { busy } => self.busy = busy,
            ConvAction::Configured { model, effort } => {
                self.model = model;
                self.effort = effort;
            }
            ConvAction::Failed(message) => self.error = Some(message),
        }
    }
}

/// Append a delta, extending the trailing item when it is the same kind and
/// starting a new one otherwise — so prose either side of a tool call stays
/// two blocks rather than merging across it.
fn push_text(buf: &mut Vec<StreamItem>, delta: String, thinking: bool) {
    match (buf.last_mut(), thinking) {
        (Some(StreamItem::Text(prev)), false) => prev.push_str(&delta),
        (Some(StreamItem::Thinking(prev)), true) => prev.push_str(&delta),
        (_, false) => buf.push(StreamItem::Text(delta)),
        (_, true) => buf.push(StreamItem::Thinking(delta)),
    }
}

/// Add a live-delivered entry, unless the transcript already has it.
///
/// Identity is the timestamp. The server composes an entry once, when it
/// accepts the message, and that stamp survives to disk — so the copy pushed
/// down the event feed and the copy a later poll returns are the same record,
/// and the client can tell without the protocol carrying an id.
fn merge(entries: &mut Vec<api::Entry>, entry: api::Entry) {
    if entries.iter().any(|e| e.at == entry.at) {
        return;
    }
    entries.push(entry);
}

/// Fold a poll response into what is already on screen.
///
/// The server's copy wins, but a message we were told about over the feed and
/// that has not been persisted yet must survive the poll — otherwise a
/// message posted mid-turn appears, vanishes on the next tick, and comes back
/// when the turn ends. Only entries newer than anything the server knows are
/// kept, so one the server deliberately dropped does not live forever.
fn reconcile(local: &mut Vec<api::Entry>, server: Vec<api::Entry>) {
    let newest = server.iter().map(|e| e.at).max();
    let pending: Vec<api::Entry> = local
        .iter()
        .filter(|e| newest.is_none_or(|n| e.at > n))
        .filter(|e| !server.iter().any(|s| s.at == e.at))
        .cloned()
        .collect();
    *local = server;
    local.extend(pending);
}

/// Has anyone but me posted here? A session with a second person in it is a
/// room, and the agent only answers when it is named — so the composer says so.
pub fn is_shared(entries: &[api::Entry], me: Option<&api::Identity>) -> bool {
    entries.iter().any(|e| match (&e.author, me) {
        (api::Author::User { id, .. }, Some(i)) => *id != i.id,
        (api::Author::User { .. }, None) => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Option<api::Identity> {
        Some(api::Identity {
            id: "ada".into(),
            name: "Ada Lovelace".into(),
        })
    }

    fn msg(id: &str, text: &str) -> api::Entry {
        api::Entry::user(
            api::Author::User {
                id: id.into(),
                name: id.into(),
            },
            vec![api::Content::Text { text: text.into() }],
        )
    }

    fn poll(entries: Vec<api::Entry>, busy: bool) -> api::Poll {
        api::Poll {
            busy,
            total: entries.len(),
            entries,
            last_error: None,
        }
    }

    /// The regression this module exists for: a message arriving on the feed
    /// joins the transcript that is actually on screen. (The old
    /// `use_state`-captured handles merged into the *first render's* empty
    /// transcript, so every arrival wiped the screen down to itself.)
    #[test]
    fn a_delivered_message_joins_the_transcript_on_screen() {
        let mut st = ConvState::new(me());
        st.apply(ConvAction::Loaded {
            entries: vec![msg("ada", "one"), msg("ada", "two")],
            busy: false,
        });
        st.apply(ConvAction::Message {
            entry: msg("grace", "three"),
            wakes_agent: false,
        });
        assert_eq!(st.entries.len(), 3, "nothing on screen may be lost");
        assert_eq!(st.entries[2].text(), "three");
        assert!(st.shared, "someone else posted");
        assert!(!st.busy, "a room note does not start a turn");
    }

    /// Delivered-but-unpersisted messages survive the slow poll, and stop
    /// being "pending" the moment the server's copy arrives (same timestamp).
    #[test]
    fn a_delivered_message_survives_polls_until_the_server_has_it() {
        let mut st = ConvState::new(me());
        let older = msg("ada", "hello");
        st.apply(ConvAction::Loaded {
            entries: vec![older.clone()],
            busy: true,
        });
        let note = msg("grace", "posted mid-turn");
        st.apply(ConvAction::Message {
            entry: note.clone(),
            wakes_agent: false,
        });

        // A poll that does not know the note yet must not erase it. (It was
        // delivered mid-turn, so it is held as an arrival, still on screen.)
        st.apply(ConvAction::Polled {
            poll: poll(vec![older.clone()], true),
        });
        let on_screen = |st: &ConvState| {
            st.entries
                .iter()
                .chain(st.arrivals.iter())
                .filter(|e| e.at == note.at)
                .count()
        };
        assert_eq!(on_screen(&st), 1, "note vanished");

        // Once the server returns it, there is exactly one copy — in the
        // transcript, not still held as an arrival too.
        st.apply(ConvAction::Polled {
            poll: poll(vec![older, note.clone()], false),
        });
        assert_eq!(on_screen(&st), 1);
        assert!(st.arrivals.is_empty(), "absorbed arrivals must not linger");
    }

    /// Mid-turn, the checkpointed entries cover output the stream buffer
    /// still shows; adopting them would render the same content twice. The
    /// poll updates the busy flag and nothing else while streaming.
    #[test]
    fn the_slow_poll_stands_down_while_a_turn_streams() {
        let mut st = ConvState::new(me());
        st.apply(ConvAction::Loaded {
            entries: vec![msg("ada", "go")],
            busy: true,
        });
        st.apply(ConvAction::Text("partial answer".into()));

        let checkpoint = vec![msg("ada", "go"), msg("ada", "checkpointed")];
        st.apply(ConvAction::Polled {
            poll: poll(checkpoint, true),
        });
        assert_eq!(st.entries.len(), 1, "mid-turn poll must not be adopted");
        assert!(st.busy);
    }

    /// A tool call streams as a running card and its result completes that
    /// card in place — same position, same identity, no shuffle.
    #[test]
    fn a_tool_result_completes_its_running_card_in_place() {
        let mut st = ConvState::new(me());
        st.apply(ConvAction::Text("let me check".into()));
        st.apply(ConvAction::ToolStarted {
            id: "call_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"cmd": "ls"}),
        });
        st.apply(ConvAction::Text("and meanwhile".into()));
        let before: Vec<usize> = (0..st.streaming.len()).collect();

        st.apply(ConvAction::ToolFinished {
            result: api::ToolResult::text("files").with_id("call_1"),
        });
        assert_eq!((0..st.streaming.len()).collect::<Vec<_>>(), before);
        match &st.streaming[1] {
            StreamItem::Tool { id, result, .. } => {
                assert_eq!(id, "call_1");
                assert!(result.is_some(), "the card completed in place");
            }
            other => panic!("expected the tool card, got {:?}", other.size()),
        }
    }

    /// The buffer and the transcript swap in one step at turn end: the
    /// streamed content never disappears before its saved form shows.
    #[test]
    fn turn_end_swaps_buffer_for_persisted_entries_in_one_step() {
        let mut st = ConvState::new(me());
        st.apply(ConvAction::TurnStarted);
        st.apply(ConvAction::Text("the answer".into()));
        assert!(st.busy);

        st.apply(ConvAction::TurnEnded {
            poll: poll(vec![msg("ada", "go"), msg("ada", "the answer")], false),
        });
        assert!(st.streaming.is_empty());
        assert_eq!(st.entries.len(), 2);
        assert!(!st.busy);
    }

    /// An unmatched result (stream joined mid-turn) has no card to complete
    /// and must not panic or misfile onto another call's card.
    #[test]
    fn an_unmatched_tool_result_is_ignored() {
        let mut st = ConvState::new(me());
        st.apply(ConvAction::ToolStarted {
            id: "call_other".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        });
        st.apply(ConvAction::ToolFinished {
            result: api::ToolResult::text("stray").with_id("call_unknown"),
        });
        match &st.streaming[0] {
            StreamItem::Tool { result, .. } => assert!(result.is_none()),
            other => panic!("expected the tool card, got {:?}", other.size()),
        }
    }

    /// A message delivered while a turn streams renders after the stream
    /// buffer (see `ConvState::arrivals`), and joins the transcript exactly
    /// once when the turn ends.
    #[test]
    fn a_mid_turn_message_holds_position_after_the_stream_buffer() {
        let mut st = ConvState::new(me());
        st.apply(ConvAction::Loaded {
            entries: vec![msg("ada", "go")],
            busy: true,
        });
        st.apply(ConvAction::Text("partial answer".into()));
        let note = msg("grace", "mid-turn note");
        st.apply(ConvAction::Message {
            entry: note.clone(),
            wakes_agent: false,
        });
        assert_eq!(st.entries.len(), 1, "must not appear above the stream");
        assert_eq!(st.arrivals.len(), 1, "renders after the stream buffer");

        // The persisted turn absorbs it, in the server's order, one copy.
        let saved = vec![msg("ada", "go"), msg("ada", "partial answer"), note.clone()];
        st.apply(ConvAction::TurnEnded {
            poll: poll(saved, false),
        });
        assert!(st.arrivals.is_empty());
        assert_eq!(st.entries.iter().filter(|e| e.at == note.at).count(), 1);
        assert_eq!(st.entries[2].at, note.at, "server order wins");
    }

    /// Duplicate delivery (feed + poll, reconnect replays) folds to one copy.
    #[test]
    fn merge_dedupes_on_the_timestamp() {
        let mut st = ConvState::new(me());
        let entry = msg("grace", "once");
        st.apply(ConvAction::Message {
            entry: entry.clone(),
            wakes_agent: false,
        });
        st.apply(ConvAction::Message {
            entry,
            wakes_agent: false,
        });
        assert_eq!(st.entries.len(), 1);
    }
}
