//! The multiplayer room: who has posted in a session, and what has been
//! accepted but not yet folded into history. One lock covers the wake
//! decision, the feed broadcast, and the inbox push (see `post_message`),
//! so acceptance order is feed order is fold order.

use myco_api as api;
use myco_api::{Author, Entry, EntryBody};

/// A message the room has accepted but the history has not absorbed yet.
pub(super) struct Accepted {
    pub(super) entry: Entry,
    pub(super) wakes_agent: bool,
}

/// Post-acceptance state for one live session: who has posted here, and what
/// has been accepted but not yet folded into history. One lock covers the
/// wake decision, the feed broadcast, and the inbox push, so acceptance order
/// is feed order is fold order — and the room rule is never computed from a
/// stale snapshot while messages sit queued behind a running turn.
pub(super) struct Room {
    /// User ids that have posted. Agent and system entries do not make a room
    /// — only other people do.
    pub(super) participants: std::collections::HashSet<String>,
    /// Accepted messages the agent task (or the running turn's boundary
    /// drain) has yet to fold, in acceptance order.
    pub(super) inbox: std::collections::VecDeque<Accepted>,
}

impl Room {
    pub(super) fn seeded(entries: &[Entry]) -> Self {
        Self {
            participants: Self::participants_of(entries),
            inbox: std::collections::VecDeque::new(),
        }
    }

    pub(super) fn participants_of(entries: &[Entry]) -> std::collections::HashSet<String> {
        entries
            .iter()
            .filter(|e| matches!(&e.body, EntryBody::User { .. }))
            .filter_map(|e| match &e.author {
                Author::User { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Should this message wake the agent?
    ///
    /// The rule is explicit address, with one carve-out: a session nobody else
    /// has posted in is a private line, and everything said there is said to
    /// the agent. The moment a second person joins, the room becomes a room —
    /// the agent answers when it is called (`@myco`, `@agent`, `@assistant`,
    /// or the model key it is running as) and otherwise listens.
    ///
    /// Deliberately not a judgement call about intent. In a room, an agent
    /// that guesses wrong talks over people; one that waits to be named never
    /// does.
    pub(super) fn wakes_agent(&self, text: &str, model: &str, author: &Author) -> bool {
        if api::mention::addresses_agent(text, model) {
            return true;
        }
        !self.is_shared(author)
    }

    /// Has anyone other than `author` posted here?
    pub(super) fn is_shared(&self, author: &Author) -> bool {
        match author {
            Author::User { id, .. } => self.participants.iter().any(|p| p != id),
            // A non-human poster (subagent plumbing) never turns a session
            // shared, and is never gated by one.
            _ => false,
        }
    }
}
