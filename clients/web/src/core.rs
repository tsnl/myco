//! The client is a fold (DESIGN.md L3): one [`State`], one [`Action`]
//! stream, one [`reduce`] — `State × Action → Effects`. Everything that
//! happens to the UI happens here; the wasm layer renders the state and
//! runs the effects, nothing more.
//!
//! Deliberately free of wasm and DOM: this module compiles and tests on
//! the native target, and is the part a native client (DP‑1) would reuse
//! whole. The action log rides the state — a client bug replays as a fold
//! over recorded actions.

/// Everything the client knows. Rendering is a pure function of this.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub session: Session,
    /// Recent actions, newest last, capped — the repro log.
    pub log: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Session {
    /// Before the first probe answers.
    #[default]
    Checking,
    /// The server answered 401: reachable, and honest about anonymity.
    SignedOut,
    SignedIn(User),
    /// The server did not answer (or answered strangely).
    Unreachable {
        why: String,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// Everything that can happen, from every source — user input, wire
/// answers, and (in later PRs) the watch feed — one vocabulary, one queue.
#[derive(Debug, Clone)]
pub enum Action {
    /// The app started.
    Boot,
    /// `GET /api/whoami` answered.
    WhoamiAnswered { status: u16, body: String },
    /// A wire call failed before an HTTP status existed.
    NetworkFailed { what: String },
}

/// What the edge must do. Effects re-enter as actions; they never touch
/// state themselves.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// `GET /api/whoami` → [`Action::WhoamiAnswered`] / [`Action::NetworkFailed`].
    Whoami,
}

const LOG_CAP: usize = 256;

pub fn reduce(state: &mut State, action: Action) -> Vec<Effect> {
    if state.log.len() == LOG_CAP {
        state.log.remove(0);
    }
    state.log.push(format!("{action:?}"));

    match action {
        Action::Boot => {
            state.session = Session::Checking;
            vec![Effect::Whoami]
        }
        Action::WhoamiAnswered { status, body } => {
            state.session = match status {
                200 => match serde_json::from_str::<User>(&body) {
                    Ok(user) => Session::SignedIn(user),
                    Err(_) => Session::Unreachable {
                        why: "the server's whoami made no sense".into(),
                    },
                },
                401 => Session::SignedOut,
                other => Session::Unreachable {
                    why: format!("the server answered {other}"),
                },
            };
            vec![]
        }
        Action::NetworkFailed { what } => {
            state.session = Session::Unreachable {
                why: format!("no answer from the server ({what})"),
            };
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot() -> (State, Vec<Effect>) {
        let mut state = State::default();
        let effects = reduce(&mut state, Action::Boot);
        (state, effects)
    }

    #[test]
    fn boot_probes_and_a_401_lands_signed_out() {
        let (mut state, effects) = boot();
        assert_eq!(effects, vec![Effect::Whoami]);
        assert_eq!(state.session, Session::Checking);

        let effects = reduce(
            &mut state,
            Action::WhoamiAnswered {
                status: 401,
                body: r#"{"error":"unauthorized"}"#.into(),
            },
        );
        assert!(effects.is_empty());
        assert_eq!(state.session, Session::SignedOut);
    }

    #[test]
    fn a_live_token_boots_straight_into_a_session() {
        let (mut state, _) = boot();
        reduce(
            &mut state,
            Action::WhoamiAnswered {
                status: 200,
                body: r#"{"id":"ada","name":"Ada Lovelace"}"#.into(),
            },
        );
        assert_eq!(
            state.session,
            Session::SignedIn(User {
                id: "ada".into(),
                name: "Ada Lovelace".into(),
            })
        );
    }

    #[test]
    fn failures_read_unreachable_with_the_reason() {
        let (mut state, _) = boot();
        reduce(
            &mut state,
            Action::NetworkFailed {
                what: "fetch".into(),
            },
        );
        let Session::Unreachable { why } = &state.session else {
            panic!("expected unreachable, got {:?}", state.session);
        };
        assert!(why.contains("fetch"), "{why}");

        reduce(
            &mut state,
            Action::WhoamiAnswered {
                status: 500,
                body: String::new(),
            },
        );
        assert!(matches!(&state.session, Session::Unreachable { why } if why.contains("500")));
    }

    /// The log is the repro: every action lands in order, capped.
    #[test]
    fn the_action_log_records_everything_and_stays_bounded() {
        let mut state = State::default();
        for _ in 0..300 {
            reduce(
                &mut state,
                Action::NetworkFailed {
                    what: "tick".into(),
                },
            );
        }
        assert_eq!(state.log.len(), 256);
        assert!(state.log.last().unwrap().contains("NetworkFailed"));
    }
}
