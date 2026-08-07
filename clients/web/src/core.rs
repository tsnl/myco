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
    /// The live bearer token. In state (not hidden in the edge) because
    /// effects need it and the reducer owns what effects see.
    pub token: Option<String>,
    pub sign_in: SignIn,
    /// Feedback under the signed-in card's passkey button.
    pub passkey_note: Option<String>,
    /// The workspace: what the pool holds, kept fresh by list + feed.
    pub workspace: Workspace,
    /// Recent actions, newest last, capped — the repro log.
    pub log: Vec<String>,
}

/// The pool as the client knows it. `instances` is a cache of the listing,
/// refreshed whenever the event feed hints at change — the feed is lossy
/// by doctrine, so re-listing *is* the recovery path, not a fallback.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Workspace {
    pub kinds: Vec<KindInfo>,
    pub instances: Vec<InstanceInfo>,
    pub selected: Option<String>,
    pub feed: Feed,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Feed {
    #[default]
    Connecting,
    Live,
    /// The socket dropped: showing last state, retrying.
    Reconnecting,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct KindInfo {
    pub kind: String,
    #[serde(default)]
    pub doc: String,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct InstanceInfo {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub title: String,
    pub creator: PrincipalRef,
    #[serde(default)]
    pub driver: Option<PrincipalRef>,
    #[serde(default)]
    pub crashed: bool,
}

/// A principal on the wire: {kind: human|agent|system, id}.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct PrincipalRef {
    pub kind: String,
    pub id: String,
}

/// The sign-in island's own state: in flight, and the last refusal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SignIn {
    pub busy: bool,
    pub error: Option<String>,
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

impl User {
    pub fn display(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }
}

/// Everything that can happen, from every source — user input, wire
/// answers, and (in later PRs) the watch feed — one vocabulary, one queue.
#[derive(Debug, Clone)]
pub enum Action {
    /// The app started; `token` is whatever localStorage remembered.
    Boot { token: Option<String> },
    /// `GET /api/whoami` answered.
    WhoamiAnswered { status: u16, body: String },
    /// The person submitted the sign-in form.
    SignInSubmitted { username: String, code: String },
    /// `POST /api/auth/token` answered.
    TokenAnswered { status: u16, body: String },
    /// The person asked to sign out.
    SignOutRequested,
    /// The person asked to enroll a passkey (signed in).
    EnrollPasskeyRequested,
    /// The whole enrollment ceremony answered (the finish call's status).
    PasskeyEnrollAnswered { status: u16, body: String },
    /// The person asked to sign in with a passkey.
    PasskeySignInRequested { username: String },
    /// A browser ceremony failed or was dismissed before any wire call.
    PasskeyFailed { why: String },
    /// `GET /api/kinds` answered.
    KindsAnswered { status: u16, body: String },
    /// `GET /api/instances` answered.
    InstancesAnswered { status: u16, body: String },
    /// The event socket delivered a pool event (any name) — a hint that
    /// the listing may be stale.
    FeedEvent,
    /// The event socket opened.
    FeedOpened,
    /// The event socket closed or errored; the edge will retry.
    FeedDropped,
    /// The person selected an instance in the tree.
    Selected { id: String },
    /// The person asked to create an instance of `kind`.
    CreateRequested { kind: String },
    /// `POST /api/instances` answered.
    CreateAnswered { status: u16, body: String },
    /// A wire call failed before an HTTP status existed.
    NetworkFailed { what: String },
}

/// What the edge must do. Effects re-enter as actions; they never touch
/// state themselves.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// `GET /api/whoami` (with the token when present) →
    /// [`Action::WhoamiAnswered`].
    Whoami { token: Option<String> },
    /// `POST /api/auth/token` (`grant_type=code`) →
    /// [`Action::TokenAnswered`].
    RedeemCode { username: String, code: String },
    /// Remember the token across reloads.
    PersistToken(String),
    /// Forget the remembered token.
    ClearToken,
    /// `POST /api/auth/logout`, best-effort — signing out locally never
    /// waits on the wire.
    Logout { token: String },
    /// The whole enrollment ceremony: register/start → the browser's
    /// `credentials.create` → register/finish → [`Action::PasskeyEnrollAnswered`]
    /// (or [`Action::PasskeyFailed`] if the browser bows out).
    EnrollPasskey { token: String },
    /// The whole login ceremony: login/start → `credentials.get` →
    /// login/finish → [`Action::TokenAnswered`] — the finish answers in the
    /// code grant's exact shape, so sign-in converges downstream.
    PasskeySignIn { username: String },
    /// `GET /api/kinds` → [`Action::KindsAnswered`].
    FetchKinds { token: String },
    /// `GET /api/instances` → [`Action::InstancesAnswered`].
    FetchInstances { token: String },
    /// Open (or reopen) the event socket; delivers [`Action::FeedOpened`],
    /// [`Action::FeedEvent`]s, and one [`Action::FeedDropped`] at close.
    OpenFeed { token: String },
    /// `POST /api/instances` with `{kind}` → [`Action::CreateAnswered`].
    CreateInstance { token: String, kind: String },
}

/// What entering a session kicks off, from either door (whoami or a fresh
/// token): the workspace loads and the feed opens.
fn enter_workspace(token: &str) -> Vec<Effect> {
    vec![
        Effect::FetchKinds {
            token: token.into(),
        },
        Effect::FetchInstances {
            token: token.into(),
        },
        Effect::OpenFeed {
            token: token.into(),
        },
    ]
}

/// The token endpoint's 200 body (RFC 6749's shape).
#[derive(Debug, Clone, serde::Deserialize)]
struct IssuedToken {
    access_token: String,
    user: User,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Refusal {
    #[serde(default)]
    why: String,
}

const LOG_CAP: usize = 256;

pub fn reduce(state: &mut State, action: Action) -> Vec<Effect> {
    if state.log.len() == LOG_CAP {
        state.log.remove(0);
    }
    state.log.push(format!("{action:?}"));

    match action {
        Action::Boot { token } => {
            state.session = Session::Checking;
            state.token = token.clone();
            vec![Effect::Whoami { token }]
        }
        Action::WhoamiAnswered { status, body } => {
            match status {
                200 => match serde_json::from_str::<User>(&body) {
                    Ok(user) => {
                        state.session = Session::SignedIn(user);
                        if let Some(token) = &state.token {
                            return enter_workspace(token);
                        }
                    }
                    Err(_) => {
                        state.session = Session::Unreachable {
                            why: "the server's whoami made no sense".into(),
                        }
                    }
                },
                401 => {
                    state.session = Session::SignedOut;
                    // A remembered token the server refused is stale:
                    // forget it rather than re-presenting it forever.
                    if state.token.take().is_some() {
                        return vec![Effect::ClearToken];
                    }
                }
                other => {
                    state.session = Session::Unreachable {
                        why: format!("the server answered {other}"),
                    }
                }
            }
            vec![]
        }
        Action::SignInSubmitted { username, code } => {
            let username = username.trim().to_string();
            let code = code.trim().to_string();
            if username.is_empty() || code.is_empty() {
                state.sign_in.error = Some("both fields, please".into());
                return vec![];
            }
            state.sign_in.busy = true;
            state.sign_in.error = None;
            vec![Effect::RedeemCode { username, code }]
        }
        Action::TokenAnswered { status, body } => {
            state.sign_in.busy = false;
            match status {
                200 => match serde_json::from_str::<IssuedToken>(&body) {
                    Ok(issued) => {
                        state.token = Some(issued.access_token.clone());
                        state.session = Session::SignedIn(issued.user);
                        state.sign_in = SignIn::default();
                        let mut effects = vec![Effect::PersistToken(issued.access_token.clone())];
                        effects.extend(enter_workspace(&issued.access_token));
                        effects
                    }
                    Err(_) => {
                        state.sign_in.error =
                            Some("the server's token answer made no sense".into());
                        vec![]
                    }
                },
                _ => {
                    let why = serde_json::from_str::<Refusal>(&body)
                        .ok()
                        .map(|r| r.why)
                        .filter(|w| !w.is_empty())
                        .unwrap_or_else(|| "sign-in refused".into());
                    state.sign_in.error = Some(why);
                    vec![]
                }
            }
        }
        Action::EnrollPasskeyRequested => {
            state.passkey_note = None;
            match &state.token {
                Some(token) => vec![Effect::EnrollPasskey {
                    token: token.clone(),
                }],
                None => vec![],
            }
        }
        Action::PasskeyEnrollAnswered { status, body } => {
            #[derive(serde::Deserialize)]
            struct Enrolled {
                passkeys: u64,
            }
            state.passkey_note = Some(match status {
                200 => match serde_json::from_str::<Enrolled>(&body) {
                    Ok(e) => format!("passkey added — {} on file", e.passkeys),
                    Err(_) => "passkey added".into(),
                },
                _ => serde_json::from_str::<Refusal>(&body)
                    .ok()
                    .map(|r| r.why)
                    .filter(|w| !w.is_empty())
                    .unwrap_or_else(|| "enrollment refused".into()),
            });
            vec![]
        }
        Action::PasskeySignInRequested { username } => {
            let username = username.trim().to_string();
            if username.is_empty() {
                state.sign_in.error = Some("a passkey signs in a username — fill it in".into());
                return vec![];
            }
            state.sign_in.busy = true;
            state.sign_in.error = None;
            vec![Effect::PasskeySignIn { username }]
        }
        Action::PasskeyFailed { why } => {
            if state.sign_in.busy {
                state.sign_in.busy = false;
                state.sign_in.error = Some(why);
            } else {
                state.passkey_note = Some(why);
            }
            vec![]
        }
        Action::SignOutRequested => {
            let mut effects = vec![Effect::ClearToken];
            if let Some(token) = state.token.take() {
                effects.push(Effect::Logout { token });
            }
            state.session = Session::SignedOut;
            state.sign_in = SignIn::default();
            state.passkey_note = None;
            state.workspace = Workspace::default();
            effects
        }
        Action::KindsAnswered { status, body } => {
            if status == 200
                && let Ok(kinds) = serde_json::from_str::<Vec<KindInfo>>(&body)
            {
                state.workspace.kinds = kinds;
            }
            vec![]
        }
        Action::InstancesAnswered { status, body } => {
            if status == 200
                && let Ok(mut instances) = serde_json::from_str::<Vec<InstanceInfo>>(&body)
            {
                // Stable order: project, then title, then id — the tree
                // must not shuffle underfoot on every refresh.
                instances.sort_by(|a, b| {
                    (&a.project, &a.title, &a.id).cmp(&(&b.project, &b.title, &b.id))
                });
                state.workspace.instances = instances;
                if let Some(selected) = &state.workspace.selected
                    && !state.workspace.instances.iter().any(|i| &i.id == selected)
                {
                    state.workspace.selected = None;
                }
            }
            vec![]
        }
        Action::FeedEvent => match &state.token {
            // Any event is only a hint; the listing is the truth. Lossy
            // feed + re-list is the doctrine's recovery, used as the
            // ordinary path.
            Some(token) => vec![Effect::FetchInstances {
                token: token.clone(),
            }],
            None => vec![],
        },
        Action::FeedOpened => {
            state.workspace.feed = Feed::Live;
            // Anything could have happened while the socket was down.
            match &state.token {
                Some(token) => vec![Effect::FetchInstances {
                    token: token.clone(),
                }],
                None => vec![],
            }
        }
        Action::FeedDropped => {
            if matches!(state.session, Session::SignedIn(_)) {
                state.workspace.feed = Feed::Reconnecting;
            }
            vec![]
        }
        Action::Selected { id } => {
            state.workspace.selected = Some(id);
            vec![]
        }
        Action::CreateRequested { kind } => match &state.token {
            Some(token) => vec![Effect::CreateInstance {
                token: token.clone(),
                kind,
            }],
            None => vec![],
        },
        Action::CreateAnswered { status, body } => {
            if status == 200
                && let Ok(info) = serde_json::from_str::<InstanceInfo>(&body)
            {
                state.workspace.selected = Some(info.id);
            }
            match &state.token {
                Some(token) => vec![Effect::FetchInstances {
                    token: token.clone(),
                }],
                None => vec![],
            }
        }
        Action::NetworkFailed { what } => {
            if state.sign_in.busy {
                // A failed sign-in call lands on the form, not the shell.
                state.sign_in.busy = false;
                state.sign_in.error = Some(format!("no answer from the server ({what})"));
            } else {
                state.session = Session::Unreachable {
                    why: format!("no answer from the server ({what})"),
                };
            }
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn booted_signed_out() -> State {
        let mut state = State::default();
        reduce(&mut state, Action::Boot { token: None });
        reduce(
            &mut state,
            Action::WhoamiAnswered {
                status: 401,
                body: String::new(),
            },
        );
        state
    }

    #[test]
    fn boot_probes_with_the_remembered_token() {
        let mut state = State::default();
        let effects = reduce(
            &mut state,
            Action::Boot {
                token: Some("tok".into()),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::Whoami {
                token: Some("tok".into())
            }]
        );
        assert_eq!(state.session, Session::Checking);
    }

    #[test]
    fn a_stale_remembered_token_is_forgotten_on_401() {
        let mut state = State::default();
        reduce(
            &mut state,
            Action::Boot {
                token: Some("stale".into()),
            },
        );
        let effects = reduce(
            &mut state,
            Action::WhoamiAnswered {
                status: 401,
                body: String::new(),
            },
        );
        assert_eq!(effects, vec![Effect::ClearToken]);
        assert_eq!(state.token, None);
        assert_eq!(state.session, Session::SignedOut);
    }

    #[test]
    fn the_code_grant_round_trip_signs_in_and_persists() {
        let mut state = booted_signed_out();
        let effects = reduce(
            &mut state,
            Action::SignInSubmitted {
                username: " ada ".into(),
                code: "AAAAA-BBBBB".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::RedeemCode {
                username: "ada".into(),
                code: "AAAAA-BBBBB".into()
            }]
        );
        assert!(state.sign_in.busy);

        let effects = reduce(
            &mut state,
            Action::TokenAnswered {
                status: 200,
                body: r#"{"access_token":"tok-1","token_type":"bearer",
                          "expires_in":43200,"user":{"id":"ada","name":"Ada"}}"#
                    .into(),
            },
        );
        assert_eq!(effects[0], Effect::PersistToken("tok-1".into()));
        assert!(
            effects.contains(&Effect::OpenFeed {
                token: "tok-1".into()
            }),
            "signing in enters the workspace: {effects:?}"
        );
        assert_eq!(state.token.as_deref(), Some("tok-1"));
        assert!(matches!(&state.session, Session::SignedIn(u) if u.id == "ada"));
    }

    #[test]
    fn a_refusal_lands_on_the_form_with_the_servers_words() {
        let mut state = booted_signed_out();
        reduce(
            &mut state,
            Action::SignInSubmitted {
                username: "ada".into(),
                code: "WRONG-WRONG".into(),
            },
        );
        reduce(
            &mut state,
            Action::TokenAnswered {
                status: 401,
                body: r#"{"error":"unauthorized","why":"incorrect username or code"}"#.into(),
            },
        );
        assert!(!state.sign_in.busy);
        assert_eq!(
            state.sign_in.error.as_deref(),
            Some("incorrect username or code")
        );
        assert_eq!(state.session, Session::SignedOut, "still out");
    }

    #[test]
    fn empty_fields_refuse_locally_without_a_wire_call() {
        let mut state = booted_signed_out();
        let effects = reduce(
            &mut state,
            Action::SignInSubmitted {
                username: "".into(),
                code: "x".into(),
            },
        );
        assert!(effects.is_empty());
        assert!(state.sign_in.error.is_some());
    }

    #[test]
    fn sign_out_clears_locally_and_revokes_best_effort() {
        let mut state = booted_signed_out();
        reduce(
            &mut state,
            Action::TokenAnswered {
                status: 200,
                body: r#"{"access_token":"tok-1","user":{"id":"ada"}}"#.into(),
            },
        );
        let effects = reduce(&mut state, Action::SignOutRequested);
        assert_eq!(
            effects,
            vec![
                Effect::ClearToken,
                Effect::Logout {
                    token: "tok-1".into()
                }
            ]
        );
        assert_eq!(state.token, None);
        assert_eq!(state.session, Session::SignedOut);
    }

    fn signed_in() -> State {
        let mut state = booted_signed_out();
        reduce(
            &mut state,
            Action::TokenAnswered {
                status: 200,
                body: r#"{"access_token":"tok-1","user":{"id":"ada"}}"#.into(),
            },
        );
        state
    }

    #[test]
    fn enrollment_is_one_effect_and_lands_a_note() {
        let mut state = signed_in();
        let effects = reduce(&mut state, Action::EnrollPasskeyRequested);
        assert_eq!(
            effects,
            vec![Effect::EnrollPasskey {
                token: "tok-1".into()
            }]
        );
        reduce(
            &mut state,
            Action::PasskeyEnrollAnswered {
                status: 200,
                body: r#"{"passkeys":2}"#.into(),
            },
        );
        assert_eq!(
            state.passkey_note.as_deref(),
            Some("passkey added — 2 on file")
        );
    }

    #[test]
    fn passkey_sign_in_needs_a_username_and_converges_on_token_answered() {
        let mut state = booted_signed_out();
        let effects = reduce(
            &mut state,
            Action::PasskeySignInRequested {
                username: "  ".into(),
            },
        );
        assert!(effects.is_empty());
        assert!(state.sign_in.error.is_some());

        let effects = reduce(
            &mut state,
            Action::PasskeySignInRequested {
                username: "ada".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::PasskeySignIn {
                username: "ada".into()
            }]
        );
        // The ceremony's finish answers in the code grant's shape.
        let effects = reduce(
            &mut state,
            Action::TokenAnswered {
                status: 200,
                body: r#"{"access_token":"tok-2","user":{"id":"ada"}}"#.into(),
            },
        );
        assert_eq!(effects[0], Effect::PersistToken("tok-2".into()));
        assert!(matches!(&state.session, Session::SignedIn(_)));
    }

    #[test]
    fn a_dismissed_ceremony_lands_where_the_person_is() {
        // Mid sign-in: on the form.
        let mut state = booted_signed_out();
        reduce(
            &mut state,
            Action::PasskeySignInRequested {
                username: "ada".into(),
            },
        );
        reduce(
            &mut state,
            Action::PasskeyFailed {
                why: "the browser cancelled the passkey prompt".into(),
            },
        );
        assert!(!state.sign_in.busy);
        assert!(state.sign_in.error.as_deref().unwrap().contains("cancelled"));

        // Signed in: under the button.
        let mut state = signed_in();
        reduce(&mut state, Action::EnrollPasskeyRequested);
        reduce(
            &mut state,
            Action::PasskeyFailed {
                why: "dismissed".into(),
            },
        );
        assert_eq!(state.passkey_note.as_deref(), Some("dismissed"));
    }

    #[test]
    fn the_workspace_loads_and_the_feed_keeps_it_fresh() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: r#"[{"id":"b","kind":"tty","project":"p","title":"z",
                           "creator":{"kind":"human","id":"ada"},"driver":null},
                          {"id":"a","kind":"chat","project":"p","title":"a",
                           "creator":{"kind":"human","id":"ada"},
                           "driver":{"kind":"agent","id":"a"}}]"#
                    .into(),
            },
        );
        assert_eq!(state.workspace.instances.len(), 2);
        assert_eq!(state.workspace.instances[0].id, "a", "stable order");

        // Any feed event is a hint: re-list.
        let effects = reduce(&mut state, Action::FeedEvent);
        assert_eq!(
            effects,
            vec![Effect::FetchInstances {
                token: "tok-1".into()
            }]
        );

        // A dropped feed shows honestly and reopening re-lists.
        reduce(&mut state, Action::FeedDropped);
        assert_eq!(state.workspace.feed, Feed::Reconnecting);
        let effects = reduce(&mut state, Action::FeedOpened);
        assert_eq!(state.workspace.feed, Feed::Live);
        assert!(!effects.is_empty(), "reopening re-lists");
    }

    #[test]
    fn selection_follows_removals_and_creation_selects() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::Selected {
                id: "gone".into(),
            },
        );
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: "[]".into(),
            },
        );
        assert_eq!(state.workspace.selected, None, "a removed selection clears");

        let effects = reduce(
            &mut state,
            Action::CreateRequested {
                kind: "tty".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::CreateInstance {
                token: "tok-1".into(),
                kind: "tty".into()
            }]
        );
        reduce(
            &mut state,
            Action::CreateAnswered {
                status: 200,
                body: r#"{"id":"new-1","kind":"tty","project":"",
                          "title":"tty","creator":{"kind":"human","id":"ada"}}"#
                    .into(),
            },
        );
        assert_eq!(state.workspace.selected.as_deref(), Some("new-1"));
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
