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
    /// A listing fetch is out and its answer has not landed. Bookkeeping,
    /// not content — it lives outside [`Workspace`] so a burst of feed
    /// events cannot make the tree look changed when nothing in it is.
    pub listing_in_flight: bool,
    /// Something happened while that fetch was out, so its answer is
    /// already behind: re-list once when it lands.
    pub listing_stale: bool,
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
    /// The focused pane's instance (tree highlight rides it too).
    pub selected: Option<String>,
    /// Open panes, in split order. Layout is client state by doctrine —
    /// the server never hears about it.
    pub panes: Vec<Pane>,
    pub feed: Feed,
    /// Grouping key the next create inherits. Empty is the `workspace` bucket.
    pub current_project: String,
    /// When set, the sidebar is asking for a new project slug.
    pub project_draft: Option<String>,
}

/// One open pane: an instance id plus the last projection read. The
/// content is whatever the kind's `primary_render` verb answered — a
/// generic JSON view until kind renderers land (tty next PR).
#[derive(Debug, Clone, PartialEq)]
pub struct Pane {
    pub id: String,
    /// The `primary_render` payload, pretty-printed; `None` until the
    /// first read answers.
    pub view: Option<String>,
    /// The instance was removed or crashed while open. The pane stays —
    /// showing last state honestly beats vanishing work.
    pub gone: bool,
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
    /// The default read a pane renders (the L1 spec hint).
    #[serde(default)]
    pub primary_render: String,
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
    /// What this instance was created under — a subagent chat names the
    /// chat that spawned it. L1 identity, so the tree nests on it rather
    /// than asking any kind where it came from.
    #[serde(default)]
    pub parent: Option<String>,
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

/// Who holds the driver seat, as the client presents it.
///
/// The tree's row dot, a pane's chip, and the palette's reason for greying
/// a driven verb are three views of one fact. Spelled three times, they are
/// three chances for the client to contradict itself about who is driving —
/// so the classification happens once, here, and each view asks it for the
/// part it renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seat {
    /// Nobody: the open ring, and the take is an invitation.
    Open,
    Human(String),
    Agent,
    System,
}

/// The seat an instance's driver puts it in.
pub fn seat_of(driver: Option<&PrincipalRef>) -> Seat {
    match driver {
        None => Seat::Open,
        Some(p) if p.kind == "human" => Seat::Human(p.id.clone()),
        Some(p) if p.kind == "agent" => Seat::Agent,
        Some(_) => Seat::System,
    }
}

impl Seat {
    /// STYLE.md's presence vocabulary, as a class suffix.
    pub fn tone(&self) -> &'static str {
        match self {
            Seat::Open => "open",
            Seat::Human(_) => "human",
            Seat::Agent => "agent",
            Seat::System => "system",
        }
    }

    /// The seat in words: a pane chip's label, a palette row's reason.
    pub fn phrase(&self) -> String {
        match self {
            Seat::Open => "seat open".into(),
            Seat::Human(id) => format!("{id} driving"),
            Seat::Agent => "agent driving".into(),
            Seat::System => "system driving".into(),
        }
    }

    /// Is `me` the person in the seat? The whole of "may I drive this".
    pub fn held_by(&self, me: Option<&str>) -> bool {
        matches!(self, Seat::Human(id) if me == Some(id.as_str()))
    }
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
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
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
    /// The person selected an instance in the tree (opens its pane).
    Selected { id: String },
    /// The person closed a pane.
    PaneClosed { id: String },
    /// The watch stream marked an instance: state changed, re-read.
    Marked { id: String },
    /// A watched instance is gone (removed or crashed).
    InstanceGone { id: String },
    /// The person asked to take the seat on an instance.
    TakeRequested { id: String },
    /// The person asked to release the seat.
    ReleaseRequested { id: String },
    /// A verb call answered. One action for every verb the client calls:
    /// what differs between callers is what should happen next, and that
    /// is exactly what `origin` says.
    VerbReplied {
        origin: Origin,
        id: String,
        verb: String,
        status: u16,
        body: String,
    },
    /// The person asked to create an instance of `kind`.
    CreateRequested { kind: String },
    /// `POST /api/instances` answered.
    CreateAnswered { status: u16, body: String },
    /// The person picked a project (tree header or the chip). Empty is workspace.
    ProjectSelected { project: String },
    /// Open the new-project slug field.
    NewProjectRequested,
    /// The slug field changed.
    ProjectDrafted { draft: String },
    /// Commit the slug field as the current project.
    NewProjectCommitted { slug: String },
    /// A wire call failed before an HTTP status existed.
    NetworkFailed { what: String },
}

/// Who asked for a verb call — the only thing that distinguished one
/// answered-verb action from another. A pane's projection read, the pane
/// chrome's seat verbs, and (above this PR) the palette's run all ask the
/// gateway the same question; they differ in what they do with the answer,
/// so the reducer matches on this rather than on a variant per caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A pane's projection read: the answer *is* the pane's view.
    Pane,
    /// A pane's chrome (`sys.take` / `sys.release`): the answer is only a
    /// hint that the listing moved.
    Chrome,
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
    /// `POST /api/instances` with `{kind, project, title}` → [`Action::CreateAnswered`].
    CreateInstance {
        token: String,
        kind: String,
        project: String,
        title: String,
    },
    /// Send `{"op":"watch","id"}` on the event socket.
    Watch { id: String },
    /// Send `{"op":"unwatch","id"}` on the event socket.
    Unwatch { id: String },
    /// Call a verb on an instance → [`Action::VerbReplied`], tagged with
    /// the `origin` that asked. `args` is the JSON body, empty for none.
    /// Every verb the client calls goes through here: one wire shape, one
    /// place to get the headers and the error path right.
    CallVerb {
        origin: Origin,
        token: String,
        id: String,
        verb: String,
        args: String,
    },
}

impl Effect {
    /// A verb call with no arguments — the common case.
    fn call(origin: Origin, token: &str, id: &str, verb: &str) -> Self {
        Effect::CallVerb {
            origin,
            token: token.into(),
            id: id.into(),
            verb: verb.into(),
            args: String::new(),
        }
    }
}

/// What entering a session kicks off, from either door (whoami or a fresh
/// token): the workspace loads and the feed opens.
fn enter_workspace(state: &mut State, token: &str) -> Vec<Effect> {
    let mut effects = vec![Effect::FetchKinds {
        token: token.into(),
    }];
    effects.extend(relist(state));
    effects.push(Effect::OpenFeed {
        token: token.into(),
    });
    effects
}

/// Ask for a fresh listing — at most one at a time.
///
/// The feed is a hint that fires per event, and events arrive in bursts (a
/// chat's turn, a terminal's output). One fetch per hint is one fetch per
/// byte of somebody else's work. So: if a fetch is already out, its answer
/// is already behind — remember that, and send exactly one more when it
/// lands. Any number of hints during a fetch cost one follow-up, never N.
fn relist(state: &mut State) -> Vec<Effect> {
    let Some(token) = state.token.clone() else {
        return vec![];
    };
    if state.listing_in_flight {
        state.listing_stale = true;
        return vec![];
    }
    state.listing_in_flight = true;
    vec![Effect::FetchInstances { token }]
}

impl Workspace {
    /// The projection verb a pane on `id` reads: the kind's
    /// `primary_render` hint, straight from the specs the server serves.
    fn render_verb(&self, id: &str) -> Option<String> {
        let instance = self.instances.iter().find(|i| i.id == id)?;
        let kind = self.kinds.iter().find(|k| k.kind == instance.kind)?;
        (!kind.primary_render.is_empty()).then(|| kind.primary_render.clone())
    }
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

/// Same charset L1 will enforce: `^[A-Za-z0-9][A-Za-z0-9-]*$`.
pub fn valid_slug(title: &str) -> bool {
    let mut bytes = title.bytes();
    match bytes.next() {
        Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9') => {
            bytes.all(|c| matches!(c, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-'))
        }
        _ => false,
    }
}

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
                        if let Some(token) = state.token.clone() {
                            return enter_workspace(state, &token);
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
                        effects.extend(enter_workspace(state, &issued.access_token));
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
            state.listing_in_flight = false;
            state.listing_stale = false;
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
            state.listing_in_flight = false;
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
            // Whatever arrived while this fetch was out is owed exactly
            // one more look.
            if std::mem::take(&mut state.listing_stale) {
                return relist(state);
            }
            vec![]
        }
        // Any event is only a hint; the listing is the truth. Lossy feed +
        // re-list is the doctrine's recovery, used as the ordinary path —
        // coalesced, because a hint is not worth a fetch of its own.
        Action::FeedEvent => relist(state),
        Action::FeedOpened => {
            state.workspace.feed = Feed::Live;
            // Anything could have happened while the socket was down:
            // re-list, and re-arm every open pane's watch + re-read.
            let mut effects = relist(state);
            if let Some(token) = &state.token {
                for pane in &state.workspace.panes {
                    effects.push(Effect::Watch {
                        id: pane.id.clone(),
                    });
                    if let Some(verb) = state.workspace.render_verb(&pane.id) {
                        effects.push(Effect::call(Origin::Pane, token, &pane.id, &verb));
                    }
                }
            }
            effects
        }
        Action::FeedDropped => {
            if matches!(state.session, Session::SignedIn(_)) {
                state.workspace.feed = Feed::Reconnecting;
            }
            vec![]
        }
        Action::Selected { id } => {
            state.workspace.selected = Some(id.clone());
            if state.workspace.panes.iter().any(|p| p.id == id) {
                return vec![];
            }
            state.workspace.panes.push(Pane {
                id: id.clone(),
                view: None,
                gone: false,
            });
            let mut effects = vec![Effect::Watch { id: id.clone() }];
            if let (Some(token), Some(verb)) = (&state.token, state.workspace.render_verb(&id)) {
                effects.push(Effect::call(Origin::Pane, token, &id, &verb));
            }
            effects
        }
        Action::PaneClosed { id } => {
            state.workspace.panes.retain(|p| p.id != id);
            if state.workspace.selected.as_deref() == Some(id.as_str()) {
                state.workspace.selected = state.workspace.panes.last().map(|p| p.id.clone());
            }
            vec![Effect::Unwatch { id }]
        }
        Action::Marked { id } => {
            let watched = state.workspace.panes.iter().any(|p| p.id == id && !p.gone);
            match (watched, &state.token, state.workspace.render_verb(&id)) {
                (true, Some(token), Some(verb)) => {
                    vec![Effect::call(Origin::Pane, token, &id, &verb)]
                }
                _ => vec![],
            }
        }
        Action::InstanceGone { id } => {
            if let Some(pane) = state.workspace.panes.iter_mut().find(|p| p.id == id) {
                pane.gone = true;
            }
            match &state.token {
                Some(token) => vec![Effect::FetchInstances {
                    token: token.clone(),
                }],
                None => vec![],
            }
        }
        Action::TakeRequested { id } => match &state.token {
            Some(token) => vec![Effect::call(Origin::Chrome, token, &id, "sys.take")],
            None => vec![],
        },
        Action::ReleaseRequested { id } => match &state.token {
            Some(token) => vec![Effect::call(Origin::Chrome, token, &id, "sys.release")],
            None => vec![],
        },
        Action::VerbReplied {
            origin,
            id,
            verb: _,
            status,
            body,
        } => match origin {
            Origin::Pane => {
                if status == 200
                    && let Some(pane) = state.workspace.panes.iter_mut().find(|p| p.id == id)
                {
                    // Pretty when it parses; verbatim when it doesn't — the
                    // generic projection until kind renderers land.
                    pane.view = Some(
                        serde_json::from_str::<serde_json::Value>(&body)
                            .and_then(|v| serde_json::to_string_pretty(&v))
                            .unwrap_or(body),
                    );
                }
                vec![]
            }
            // Seat changes surface through the listing; any answer means
            // the listing may have moved.
            Origin::Chrome => relist(state),
        },
        Action::CreateRequested { kind } => match &state.token {
            Some(token) => vec![Effect::CreateInstance {
                token: token.clone(),
                project: state.workspace.current_project.clone(),
                title: kind.clone(),
                kind,
            }],
            None => vec![],
        },
        Action::ProjectSelected { project } => {
            state.workspace.current_project = project;
            state.workspace.project_draft = None;
            vec![]
        }
        Action::NewProjectRequested => {
            state.workspace.project_draft = Some(String::new());
            vec![]
        }
        Action::ProjectDrafted { draft } => {
            if state.workspace.project_draft.is_some() {
                state.workspace.project_draft = Some(draft);
            }
            vec![]
        }
        Action::NewProjectCommitted { slug } => {
            let slug = slug.trim().to_string();
            if slug.is_empty() {
                state.workspace.current_project.clear();
                state.workspace.project_draft = None;
            } else if valid_slug(&slug) {
                state.workspace.current_project = slug;
                state.workspace.project_draft = None;
            }
            // A bad slug leaves the field open — type again.
            vec![]
        }
        Action::CreateAnswered { status, body } => {
            if status == 200
                && let Ok(info) = serde_json::from_str::<InstanceInfo>(&body)
            {
                state.workspace.selected = Some(info.id);
            }
            relist(state)
        }
        Action::NetworkFailed { what } => {
            // A call that never got a status cannot answer; releasing the
            // listing latch here is what keeps one lost fetch from wedging
            // the tree forever.
            state.listing_in_flight = false;
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
        assert!(
            state
                .sign_in
                .error
                .as_deref()
                .unwrap()
                .contains("cancelled")
        );

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
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: "[]".into(),
            },
        );

        // A dropped feed shows honestly and reopening re-lists.
        reduce(&mut state, Action::FeedDropped);
        assert_eq!(state.workspace.feed, Feed::Reconnecting);
        let effects = reduce(&mut state, Action::FeedOpened);
        assert_eq!(state.workspace.feed, Feed::Live);
        assert!(!effects.is_empty(), "reopening re-lists");
    }

    /// The feed fires per event and events arrive in bursts. However many
    /// land while a listing fetch is out, exactly one more fetch follows
    /// it — two in total for the whole burst, not one per hint.
    #[test]
    fn a_burst_of_feed_events_costs_one_extra_listing_fetch() {
        let mut state = signed_in();
        assert!(state.listing_in_flight, "signing in lists once");

        let fetches: usize = (0..20)
            .map(|_| reduce(&mut state, Action::FeedEvent).len())
            .sum();
        assert_eq!(fetches, 0, "not one fetch per hint");
        assert!(state.listing_stale, "but the hints are not forgotten");

        let effects = reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: "[]".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::FetchInstances {
                token: "tok-1".into()
            }],
            "the burst is worth exactly one more look"
        );
        assert!(state.listing_in_flight && !state.listing_stale);

        let effects = reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: "[]".into(),
            },
        );
        assert!(effects.is_empty(), "and then it settles");
        assert!(!state.listing_in_flight);
    }

    /// Parentage is L1 identity, carried in the listing: the client mirrors
    /// it so the tree can nest subagents under the chat that spawned them
    /// without asking any kind where it came from.
    #[test]
    fn the_listing_carries_parentage_for_the_tree_to_nest_on() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: r#"[{"id":"root","kind":"chat","project":"p","title":"a",
                           "creator":{"kind":"human","id":"ada"}},
                          {"id":"kid","kind":"chat","project":"p","title":"b",
                           "parent":"root",
                           "creator":{"kind":"agent","id":"root"}}]"#
                    .into(),
            },
        );
        let by = |id: &str| {
            state
                .workspace
                .instances
                .iter()
                .find(|i| i.id == id)
                .cloned()
                .expect("listed")
        };
        assert_eq!(by("root").parent, None);
        assert_eq!(by("kid").parent.as_deref(), Some("root"));
    }

    /// One classification of the seat, asked three different questions.
    #[test]
    fn the_seat_reads_the_same_however_it_is_asked() {
        let human = PrincipalRef {
            kind: "human".into(),
            id: "ada".into(),
        };
        let agent = PrincipalRef {
            kind: "agent".into(),
            id: "chat-1".into(),
        };

        let seat = seat_of(Some(&human));
        assert_eq!(seat.tone(), "human");
        assert_eq!(seat.phrase(), "ada driving");
        assert!(seat.held_by(Some("ada")));
        assert!(!seat.held_by(Some("grace")));

        let seat = seat_of(Some(&agent));
        assert_eq!(seat.tone(), "agent");
        assert_eq!(seat.phrase(), "agent driving");
        assert!(!seat.held_by(Some("chat-1")), "an agent is not a person");

        let seat = seat_of(None);
        assert_eq!(seat, Seat::Open);
        assert_eq!(seat.tone(), "open");
        assert!(!seat.held_by(Some("ada")));
    }

    #[test]
    fn selection_follows_removals_and_creation_selects() {
        let mut state = signed_in();
        reduce(&mut state, Action::Selected { id: "gone".into() });
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: "[]".into(),
            },
        );
        assert_eq!(state.workspace.selected, None, "a removed selection clears");

        let effects = reduce(&mut state, Action::CreateRequested { kind: "tty".into() });
        assert_eq!(
            effects,
            vec![Effect::CreateInstance {
                token: "tok-1".into(),
                kind: "tty".into(),
                project: String::new(),
                title: "tty".into(),
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

    #[test]
    fn create_inherits_the_current_project() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::ProjectSelected {
                project: "lab".into(),
            },
        );
        assert_eq!(state.workspace.current_project, "lab");
        let effects = reduce(&mut state, Action::CreateRequested { kind: "tty".into() });
        assert_eq!(
            effects,
            vec![Effect::CreateInstance {
                token: "tok-1".into(),
                kind: "tty".into(),
                project: "lab".into(),
                title: "tty".into(),
            }]
        );

        reduce(&mut state, Action::NewProjectRequested);
        reduce(
            &mut state,
            Action::ProjectDrafted {
                draft: "has space".into(),
            },
        );
        reduce(
            &mut state,
            Action::NewProjectCommitted {
                slug: "has space".into(),
            },
        );
        assert_eq!(
            state.workspace.current_project, "lab",
            "a bad slug does not stick"
        );
        assert_eq!(state.workspace.project_draft.as_deref(), Some("has space"));

        reduce(
            &mut state,
            Action::NewProjectCommitted {
                slug: "lab-2".into(),
            },
        );
        assert_eq!(state.workspace.current_project, "lab-2");
        assert!(state.workspace.project_draft.is_none());
    }

    /// A workspace with one tty instance listed and its kind known.
    fn with_tty(mut state: State) -> State {
        reduce(
            &mut state,
            Action::KindsAnswered {
                status: 200,
                body: r#"[{"kind":"tty","doc":"","primary_render":"screen"}]"#.into(),
            },
        );
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: r#"[{"id":"t1","kind":"tty","project":"","title":"shell",
                           "creator":{"kind":"human","id":"ada"},
                           "driver":{"kind":"human","id":"ada"}}]"#
                    .into(),
            },
        );
        state
    }

    #[test]
    fn selecting_opens_a_pane_watches_and_reads_the_projection() {
        let mut state = with_tty(signed_in());
        let effects = reduce(&mut state, Action::Selected { id: "t1".into() });
        assert_eq!(
            effects,
            vec![
                Effect::Watch { id: "t1".into() },
                Effect::call(Origin::Pane, "tok-1", "t1", "screen")
            ]
        );
        assert_eq!(state.workspace.panes.len(), 1);

        // Selecting again focuses without duplicating.
        let effects = reduce(&mut state, Action::Selected { id: "t1".into() });
        assert!(effects.is_empty());
        assert_eq!(state.workspace.panes.len(), 1);
    }

    #[test]
    fn a_mark_rereads_and_the_payload_lands_in_the_pane() {
        let mut state = with_tty(signed_in());
        reduce(&mut state, Action::Selected { id: "t1".into() });
        let effects = reduce(&mut state, Action::Marked { id: "t1".into() });
        assert_eq!(
            effects,
            vec![Effect::call(Origin::Pane, "tok-1", "t1", "screen")]
        );
        // A mark for an unopened instance reads nothing.
        assert!(reduce(&mut state, Action::Marked { id: "other".into() }).is_empty());

        reduce(
            &mut state,
            Action::VerbReplied {
                origin: Origin::Pane,
                id: "t1".into(),
                verb: "screen".into(),
                status: 200,
                body: r#"{"rows": 24}"#.into(),
            },
        );
        assert!(
            state.workspace.panes[0]
                .view
                .as_deref()
                .unwrap()
                .contains("24")
        );
    }

    #[test]
    fn gone_marks_the_pane_and_close_unwatches() {
        let mut state = with_tty(signed_in());
        reduce(&mut state, Action::Selected { id: "t1".into() });
        reduce(&mut state, Action::InstanceGone { id: "t1".into() });
        assert!(
            state.workspace.panes[0].gone,
            "the pane stays, honestly gone"
        );

        let effects = reduce(&mut state, Action::PaneClosed { id: "t1".into() });
        assert_eq!(effects, vec![Effect::Unwatch { id: "t1".into() }]);
        assert!(state.workspace.panes.is_empty());
        assert_eq!(state.workspace.selected, None);
    }

    #[test]
    fn a_reopened_feed_rearms_every_pane() {
        let mut state = with_tty(signed_in());
        reduce(&mut state, Action::Selected { id: "t1".into() });
        reduce(&mut state, Action::FeedDropped);
        let effects = reduce(&mut state, Action::FeedOpened);
        assert!(effects.contains(&Effect::Watch { id: "t1".into() }));
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::CallVerb {
                origin: Origin::Pane,
                id,
                ..
            } if id == "t1"
        )));
    }

    #[test]
    fn the_seat_verbs_round_trip_through_the_listing() {
        let mut state = with_tty(signed_in());
        let effects = reduce(&mut state, Action::TakeRequested { id: "t1".into() });
        assert_eq!(
            effects,
            vec![Effect::call(Origin::Chrome, "tok-1", "t1", "sys.take")]
        );
        let effects = reduce(
            &mut state,
            Action::VerbReplied {
                origin: Origin::Chrome,
                id: "t1".into(),
                verb: "sys.take".into(),
                status: 200,
                body: String::new(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::FetchInstances {
                token: "tok-1".into()
            }],
            "the chip refreshes from the listing, the one source of seat truth"
        );
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
