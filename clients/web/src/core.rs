//! The client is a fold (DESIGN.md L3): one [`State`], one [`Action`]
//! stream, one [`reduce`] — `State × Action → Effects`. Everything that
//! happens to the UI happens here; the wasm layer renders the state and
//! runs the effects, nothing more.
//!
//! Deliberately free of wasm and DOM: this module compiles and tests on
//! the native target, and is the part a native client (DP‑1) would reuse
//! whole. The action log rides the state — a client bug replays as a fold
//! over recorded actions.

mod markdown;
pub use markdown::render_markdown;

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
    /// Feedback under the enable-notifications button.
    pub push_note: Option<String>,
    /// The workspace: what the pool holds, kept fresh by list + feed.
    pub workspace: Workspace,
    /// The model catalog as the server listed it. Empty until the first
    /// `/api/models` answer — and empty after that if the workspace is
    /// modelless.
    pub catalog: Catalog,
    /// The admin surface: `Some` exactly when the server said this person
    /// is the operator (the 200 on the listing *is* the authorization —
    /// the client never decides who operates).
    pub admin: Option<Admin>,
    /// The command palette, when summoned.
    pub palette: Option<Palette>,
    /// The last palette-run verb's answer (or refusal) — a quiet line, not
    /// an ember; dismissed on the next palette open.
    pub notice: Option<String>,
    /// Inline title edit on the selected instance, when open.
    pub renaming: Option<RenameEdit>,
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
    /// The instance's kind at open time — the renderer registry's key.
    pub kind: String,
    /// The raw `primary_render` payload; `None` until the first read
    /// answers. Renderers parse it; the generic view pretty-prints it.
    pub view: Option<String>,
    /// DECCKM from the last tty screen read: arrows send SS3, not CSI.
    pub app_cursor: bool,
    /// The last size this client asked the tty for — the loop-breaker
    /// between measure and resize.
    pub sent_size: Option<(u16, u16)>,
    /// A chat pane's composer draft (uncontrolled in the DOM; mirrored
    /// here only so a submit can read it and a send can clear it).
    pub draft: String,
    /// Last `about` for a chat pane — model + effort, so the dials can
    /// render without a second kind-specific parse of `tail`.
    pub about: Option<ChatAbout>,
    /// The instance was removed or crashed while open. The pane stays —
    /// showing last state honestly beats vanishing work.
    pub gone: bool,
}

/// The catalog as a client may list it (`GET /api/models`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub default: Option<String>,
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ModelInfo {
    pub key: String,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub default: bool,
}

/// `chat.about` — the dials a pane needs, nothing else.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ChatAbout {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
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
    /// The kind's verb vocabulary — the palette's raw material.
    #[serde(default)]
    pub verbs: Vec<VerbInfo>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct VerbInfo {
    pub name: String,
    #[serde(default)]
    pub doc: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub requires_driver: bool,
    #[serde(default)]
    pub owner_only: bool,
    #[serde(default)]
    pub cursored: bool,
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

/// The operator's panel: the roster with live state, and the one-time
/// code a mint just answered (the only moment its plaintext exists).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Admin {
    pub open: bool,
    pub users: Vec<AdminUser>,
    pub minted: Option<Minted>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct AdminUser {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub sessions: u64,
    #[serde(default)]
    pub passkeys: u64,
    #[serde(default)]
    pub operator: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Minted {
    pub username: String,
    pub code: String,
    #[serde(default)]
    pub expires_at: String,
}

/// The palette: one fuzzy list over the whole registry, or the JSON-well
/// second stage a verb that wanted args sent us to.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    pub query: String,
    pub selected: usize,
    pub stage: Stage,
}

/// An in-progress title edit. Commit calls `sys.rename`; a bad slug is
/// refused here so the wire never sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct RenameEdit {
    pub id: String,
    pub draft: String,
    pub error: Option<String>,
}

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

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    List,
    /// A run refused for want of arguments: the well, pre-filled, with
    /// the server's own words about what was missing.
    Args {
        target: Target,
        draft: String,
        error: Option<String>,
    },
}

/// What the well's arguments are for. Verbs and creates share the same
/// error-driven second stage — a `host` refusing to exist without
/// `{command}` reads exactly like a verb refusing to run without args.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Verb { id: String, verb: String },
    Create { kind: String },
}

impl Target {
    /// The word the well shows for what it is arguing about.
    pub fn label(&self) -> String {
        match self {
            Target::Verb { verb, .. } => verb.clone(),
            Target::Create { kind } => format!("new {kind}"),
        }
    }
}

/// One palette row: what it says, whether it can run, and what committing
/// it dispatches. Derived, never authored — a button is a palette entry
/// with coordinates, and this is the registry both read from.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub detail: String,
    pub group: &'static str,
    /// `None` runs; `Some(reason)` renders gated with the reason — the
    /// palette never hides a capability, it explains it.
    pub gated: Option<String>,
    pub commit: Commit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Commit {
    Jump { id: String },
    Create { kind: String },
    Verb { id: String, verb: String },
    ClosePane { id: String },
    SignOut,
    AdminPanel,
}

/// The registry, filtered by the palette's query: the focused instance's
/// verbs (from the kind specs the server serves — nothing authored), jump
/// entries for every instance, create entries for every kind, and the few
/// client commands. Gating names the seat-holder rather than hiding rows.
pub fn palette_rows(state: &State, query: &str) -> Vec<Row> {
    let ws = &state.workspace;
    let me = match &state.session {
        Session::SignedIn(user) => Some(user.id.as_str()),
        _ => None,
    };
    let mut rows = Vec::new();

    if let Some(selected) = &ws.selected
        && let Some(instance) = ws.instances.iter().find(|i| &i.id == selected)
        && let Some(kind) = ws.kinds.iter().find(|k| k.kind == instance.kind)
    {
        let seat = seat_of(instance.driver.as_ref());
        for verb in &kind.verbs {
            let gated = if verb.requires_driver {
                match &seat {
                    _ if seat.held_by(me) => None,
                    Seat::Open => Some("seat open — take it first".into()),
                    held => Some(held.phrase()),
                }
            } else if verb.owner_only && me != Some(instance.creator.id.as_str()) {
                Some(format!("{} only", instance.creator.id))
            } else {
                None
            };
            rows.push(Row {
                label: verb.name.clone(),
                detail: verb.doc.clone(),
                group: "verbs",
                gated,
                commit: Commit::Verb {
                    id: instance.id.clone(),
                    verb: verb.name.clone(),
                },
            });
        }
        // Framework verbs are not in KindSpec.verbs — inject them so a
        // button and a palette row stay one registry.
        rows.push(Row {
            label: "rename".into(),
            detail: "set {title}".into(),
            group: "verbs",
            gated: None,
            commit: Commit::Verb {
                id: instance.id.clone(),
                verb: "sys.rename".into(),
            },
        });
        rows.push(Row {
            label: "close pane".into(),
            detail: String::new(),
            group: "workspace",
            gated: None,
            commit: Commit::ClosePane {
                id: selected.clone(),
            },
        });
    }
    for instance in &ws.instances {
        let title = if instance.title.is_empty() {
            &instance.kind
        } else {
            &instance.title
        };
        rows.push(Row {
            label: format!("open {title}"),
            detail: instance.kind.clone(),
            group: "instances",
            gated: None,
            commit: Commit::Jump {
                id: instance.id.clone(),
            },
        });
    }
    for kind in &ws.kinds {
        rows.push(Row {
            label: format!("new {}", kind.kind),
            detail: kind.doc.clone(),
            group: "create",
            gated: None,
            commit: Commit::Create {
                kind: kind.kind.clone(),
            },
        });
    }
    if state.admin.is_some() {
        rows.push(Row {
            label: "admin panel".into(),
            detail: "the roster, codes, seats of power".into(),
            group: "session",
            gated: None,
            commit: Commit::AdminPanel,
        });
    }
    rows.push(Row {
        label: "sign out".into(),
        detail: String::new(),
        group: "session",
        gated: None,
        commit: Commit::SignOut,
    });

    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return rows;
    }
    rows.retain(|row| fuzzy(&row.label.to_lowercase(), &query));
    rows
}

/// What a palette-run verb's answer does: reopen as the JSON well when the
/// server says the arguments were missing, otherwise leave a notice and
/// treat the run as a hint that anything may have moved.
fn palette_answered(
    state: &mut State,
    id: String,
    verb: String,
    status: u16,
    body: String,
) -> Vec<Effect> {
    #[derive(Default, serde::Deserialize)]
    struct Wire {
        #[serde(default)]
        error: String,
        #[serde(default)]
        why: String,
    }
    let wire = serde_json::from_str::<Wire>(&body).unwrap_or_default();
    if status == 200 && verb == "sys.rename" {
        // Rename is chrome: the listing moves, the answer is not a notice.
        state.notice = None;
        return relist(state);
    }
    if status == 400 && wire.error == "bad_args" {
        // The error-driven second stage: the well opens with the server's
        // own words about what was missing.
        state.palette = Some(Palette {
            query: String::new(),
            selected: 0,
            stage: Stage::Args {
                target: Target::Verb { id, verb },
                draft: "{}".into(),
                error: Some(wire.why),
            },
        });
        return vec![];
    }
    state.notice = Some(if status == 200 {
        let pretty = serde_json::from_str::<serde_json::Value>(&body)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or(body);
        let mut line: String = format!("{verb} → {pretty}");
        if line.len() > 600 {
            line.truncate(600);
            line.push('…');
        }
        line
    } else if !wire.why.is_empty() {
        format!("{verb} refused: {}", wire.why)
    } else {
        format!("{verb} answered {status}")
    });
    // A verb may have changed anything; the watched panes re-read on their
    // marks, the listing on this.
    relist(state)
}

/// Subsequence match: every query char appears, in order. Small, honest,
/// and predictable — ranking games can come later if typing earns them.
fn fuzzy(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
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
    /// The person asked for push notifications on this device.
    EnablePushRequested,
    /// The whole subscribe dance answered, in a human sentence.
    PushEnrollAnswered { note: String },
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
    /// A key went down while a tty pane was focused and drivable. The
    /// edge has already answered the synchronous preventDefault question
    /// via [`wants_key`]; this is the committed keystroke.
    KeyPressed { key: String, ctrl: bool, alt: bool },
    /// The edge measured the focused tty pane's usable cell grid.
    PaneMeasured { id: String, cols: u16, rows: u16 },
    /// A chat composer submitted its text.
    ChatPosted { id: String, text: String },
    /// A browser pane's URL row was committed.
    NavCommitted { id: String, url: String },
    /// The person asked to cancel a chat's running turn.
    TurnCancelled { id: String },
    /// A chat's model or effort dial moved.
    ChatConfigured {
        id: String,
        model: Option<String>,
        effort: Option<String>,
    },
    /// `GET /api/models` answered.
    ModelsAnswered { status: u16, body: String },
    /// `Cmd/Ctrl+P` — summon (or dismiss, when already up) the palette.
    PaletteToggled,
    /// The palette input changed.
    PaletteQueried { query: String },
    /// Arrow movement in the list (`delta` is ±1).
    PaletteMoved { delta: i32 },
    /// Enter on the list: commit the selected row.
    PaletteCommitted,
    /// Enter in the args well: run with the drafted JSON.
    PaletteArgsCommitted { draft: String },
    /// Escape — close the palette (args stage falls back to the list).
    PaletteDismissed,
    /// `GET /api/admin/users` answered (403 = simply not the operator).
    AdminAnswered { status: u16, body: String },
    /// The operator opened or closed the panel.
    AdminToggled,
    /// The operator asked for an action on a user.
    AdminActed { user: String, act: AdminAct },
    /// An admin action answered.
    AdminActAnswered {
        user: String,
        act: AdminAct,
        status: u16,
        body: String,
    },
    /// Clicked the pane or selected tree-row title: start an inline rename.
    RenameStarted { id: String },
    /// The inline rename field changed.
    RenameDrafted { draft: String },
    /// Commit the inline rename (`sys.rename`, chrome origin).
    RenameCommitted { title: String },
    /// Escape / blur away from the inline rename.
    RenameDismissed,
    /// The person asked to create an instance of `kind`.
    CreateRequested { kind: String },
    /// `POST /api/instances` answered.
    CreateAnswered {
        kind: String,
        status: u16,
        body: String,
    },
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
    /// Driving a pane — a keystroke, a resize. These answer constantly and
    /// move nothing anyone lists, so their answers are dropped on the
    /// floor rather than costing a re-list per key.
    Drive,
    /// The palette ran a verb: the answer is a notice, or — on `bad_args` —
    /// the second stage, the JSON well carrying the server's complaint.
    Palette,
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
    /// The push dance: read the notifier's VAPID key, subscribe this
    /// browser (service worker + PushManager), register the subscription
    /// back on the notifier → [`Action::PushEnrollAnswered`].
    EnablePush { token: String, notifier: String },
    /// The whole login ceremony: login/start → `credentials.get` →
    /// login/finish → [`Action::TokenAnswered`] — the finish answers in the
    /// code grant's exact shape, so sign-in converges downstream.
    PasskeySignIn { username: String },
    /// `GET /api/kinds` → [`Action::KindsAnswered`].
    FetchKinds { token: String },
    /// `GET /api/models` → [`Action::ModelsAnswered`].
    FetchModels { token: String },
    /// `GET /api/instances` → [`Action::InstancesAnswered`].
    FetchInstances { token: String },
    /// Open (or reopen) the event socket; delivers [`Action::FeedOpened`],
    /// [`Action::FeedEvent`]s, and one [`Action::FeedDropped`] at close.
    OpenFeed { token: String },
    /// `POST /api/instances` with `{kind, project, title, args}` →
    /// [`Action::CreateAnswered`].
    CreateInstance {
        token: String,
        kind: String,
        project: String,
        title: String,
        args: String,
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
    /// `GET /api/admin/users` → [`Action::AdminAnswered`].
    FetchAdmin { token: String },
    /// One admin action on one user → [`Action::AdminActAnswered`].
    AdminAct {
        token: String,
        user: String,
        act: AdminAct,
    },
}

/// The admin verbs, exactly the v2 panel's: each is one route.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AdminAct {
    Mint,
    Disable,
    Enable,
    Revoke,
    ClearPasskeys,
    Remove,
}

impl AdminAct {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "mint" => Self::Mint,
            "disable" => Self::Disable,
            "enable" => Self::Enable,
            "revoke" => Self::Revoke,
            "clear-passkeys" => Self::ClearPasskeys,
            "remove" => Self::Remove,
            _ => return None,
        })
    }
}

/// Chords the room keeps for itself — the terminal must lose only what
/// the workspace absolutely needs (DESIGN.md L3). `Cmd/Ctrl+P` is the
/// palette (next PR); the rest of the keyboard belongs to the tty.
pub fn reserved_chord(key: &str, ctrl: bool, meta: bool) -> bool {
    (ctrl || meta) && key.eq_ignore_ascii_case("p")
}

/// The synchronous question the edge must answer before preventDefault:
/// does this keystroke belong to the focused terminal? True exactly when
/// the focused pane is a live tty whose seat this person holds and the
/// chord is not reserved.
pub fn wants_key(state: &State, key: &str, ctrl: bool, meta: bool) -> bool {
    if reserved_chord(key, ctrl, meta)
        || meta
        || state.palette.is_some()
        || state.renaming.is_some()
    {
        return false;
    }
    let Session::SignedIn(user) = &state.session else {
        return false;
    };
    let Some(selected) = &state.workspace.selected else {
        return false;
    };
    let Some(pane) = state
        .workspace
        .panes
        .iter()
        .find(|p| &p.id == selected && p.kind == "tty" && !p.gone)
    else {
        return false;
    };
    state
        .workspace
        .instances
        .iter()
        .find(|i| i.id == pane.id)
        .and_then(|i| i.driver.as_ref())
        .is_some_and(|d| d.kind == "human" && d.id == user.id)
}

/// DOM `KeyboardEvent.key` → the bytes a terminal expects. `None` for
/// keys that are not the terminal's to hear (bare modifiers, F-keys we
/// don't map). `app_cursor` is DECCKM: arrows send SS3 instead of CSI.
pub fn encode_key(key: &str, ctrl: bool, alt: bool, app_cursor: bool) -> Option<String> {
    let esc = |suffix: &str| {
        Some(if app_cursor {
            format!("\x1bO{suffix}")
        } else {
            format!("\x1b[{suffix}")
        })
    };
    let encoded = match key {
        "Enter" => Some("\r".to_string()),
        "Backspace" => Some("\x7f".to_string()),
        "Tab" => Some("\t".to_string()),
        "Escape" => Some("\x1b".to_string()),
        "ArrowUp" => return esc("A"),
        "ArrowDown" => return esc("B"),
        "ArrowRight" => return esc("C"),
        "ArrowLeft" => return esc("D"),
        "Home" => return esc("H"),
        "End" => return esc("F"),
        "Delete" => Some("\x1b[3~".to_string()),
        "PageUp" => Some("\x1b[5~".to_string()),
        "PageDown" => Some("\x1b[6~".to_string()),
        printable if printable.chars().count() == 1 => {
            let c = printable.chars().next().expect("one char");
            if ctrl {
                // Ctrl+letter → C0 control byte; anything else with Ctrl
                // is not the terminal's.
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    Some(((lower as u8 - b'a' + 1) as char).to_string())
                } else {
                    None
                }
            } else {
                Some(c.to_string())
            }
        }
        _ => None,
    };
    // Alt prefixes ESC — the meta convention every terminal speaks.
    encoded.map(|data| if alt { format!("\x1b{data}") } else { data })
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

    /// A verb call with an argument object.
    fn call_with(
        origin: Origin,
        token: &str,
        id: &str,
        verb: &str,
        args: serde_json::Value,
    ) -> Self {
        Effect::CallVerb {
            origin,
            token: token.into(),
            id: id.into(),
            verb: verb.into(),
            args: args.to_string(),
        }
    }
}

/// What entering a session kicks off, from either door (whoami or a fresh
/// token): the workspace loads and the feed opens.
fn enter_workspace(state: &mut State, token: &str) -> Vec<Effect> {
    let mut effects = vec![
        Effect::FetchKinds {
            token: token.into(),
        },
        Effect::FetchModels {
            token: token.into(),
        },
    ];
    effects.extend(relist(state));
    effects.push(Effect::OpenFeed {
        token: token.into(),
    });
    // The 403 most people get back is the answer, not an error.
    effects.push(Effect::FetchAdmin {
        token: token.into(),
    });
    effects
}

#[derive(serde::Deserialize)]
struct ModelsWire {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    models: Vec<ModelInfo>,
}

/// Create args for a kind: a chat inherits the catalog default so a new
/// conversation is modeled; everything else starts empty and lets the
/// well ask if the kind needs more.
fn create_args_for(kind: &str, catalog: &Catalog) -> String {
    if kind == "chat"
        && let Some(model) = catalog
            .default
            .clone()
            .or_else(|| {
                catalog
                    .models
                    .iter()
                    .find(|m| m.default)
                    .map(|m| m.key.clone())
            })
            .or_else(|| catalog.models.first().map(|m| m.key.clone()))
    {
        return serde_json::json!({ "model": model }).to_string();
    }
    "{}".into()
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

/// A palette commit *is* the underlying action — same reducer, so a
/// button and a palette row cannot diverge. (Logged as its own entry via
/// the ordinary path.)
fn reduce_again(state: &mut State, action: Action) -> Vec<Effect> {
    reduce(state, action)
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
        Action::EnablePushRequested => {
            state.push_note = None;
            let Session::SignedIn(user) = &state.session else {
                return vec![];
            };
            // The notifier is per-person, provisioned at sign-in; finding
            // none is worth a sentence rather than a silent no-op.
            let notifier = state
                .workspace
                .instances
                .iter()
                .find(|i| i.kind == "notifier" && i.creator.id == user.id)
                .map(|i| i.id.clone());
            match (notifier, &state.token) {
                (Some(notifier), Some(token)) => vec![Effect::EnablePush {
                    token: token.clone(),
                    notifier,
                }],
                (None, _) => {
                    state.push_note = Some("no notifier instance to register with".into());
                    vec![]
                }
                _ => vec![],
            }
        }
        Action::PushEnrollAnswered { note } => {
            state.push_note = Some(note);
            vec![]
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
            state.palette = None;
            state.renaming = None;
            state.workspace = Workspace::default();
            state.catalog = Catalog::default();
            state.admin = None;
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
        Action::ModelsAnswered { status, body } => {
            if status == 200
                && let Ok(wire) = serde_json::from_str::<ModelsWire>(&body)
            {
                state.catalog = Catalog {
                    default: wire.default,
                    models: wire.models,
                };
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
            if state.renaming.as_ref().is_some_and(|r| r.id != id) {
                state.renaming = None;
            }
            state.workspace.selected = Some(id.clone());
            if state.workspace.panes.iter().any(|p| p.id == id) {
                return vec![];
            }
            let kind = state
                .workspace
                .instances
                .iter()
                .find(|i| i.id == id)
                .map(|i| i.kind.clone())
                .unwrap_or_default();
            state.workspace.panes.push(Pane {
                id: id.clone(),
                kind: kind.clone(),
                view: None,
                app_cursor: false,
                sent_size: None,
                draft: String::new(),
                about: None,
                gone: false,
            });
            let mut effects = vec![Effect::Watch { id: id.clone() }];
            if let (Some(token), Some(verb)) = (&state.token, state.workspace.render_verb(&id)) {
                effects.push(Effect::call(Origin::Pane, token, &id, &verb));
            }
            if kind == "chat"
                && let Some(token) = &state.token
            {
                effects.push(Effect::call(Origin::Chrome, token, &id, "about"));
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
            verb,
            status,
            body,
        } => match origin {
            Origin::Pane => {
                if status == 200
                    && let Some(pane) = state.workspace.panes.iter_mut().find(|p| p.id == id)
                {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
                        pane.app_cursor = value["application_cursor"] == serde_json::json!(true);
                    }
                    // Raw payload; renderers parse it by kind at draw time.
                    pane.view = Some(body);
                }
                vec![]
            }
            // Seat changes surface through the listing; a keystroke's
            // answer moves nothing there.
            Origin::Chrome if verb == "about" => {
                if status == 200
                    && let Some(pane) = state.workspace.panes.iter_mut().find(|p| p.id == id)
                {
                    pane.about = serde_json::from_str(&body).ok();
                }
                vec![]
            }
            Origin::Chrome if verb == "configure" => {
                if status == 200
                    && let Some(pane) = state.workspace.panes.iter_mut().find(|p| p.id == id)
                {
                    pane.about = serde_json::from_str(&body).ok();
                } else if status != 200 {
                    state.notice = Some(format!("configure: {body}"));
                }
                vec![]
            }
            Origin::Chrome => relist(state),
            Origin::Drive => vec![],
            Origin::Palette => palette_answered(state, id, verb, status, body),
        },
        Action::NavCommitted { id, url } => {
            let url = url.trim().to_string();
            if url.is_empty() {
                return vec![];
            }
            // A bare host gets a scheme; anything explicit passes through.
            let url = if url.contains("://") || url.starts_with("data:") {
                url
            } else {
                format!("https://{url}")
            };
            match &state.token {
                // Driving, like a keystroke: the page's own state is the
                // answer, so the reply is dropped and the watch re-reads.
                Some(token) => vec![Effect::CallVerb {
                    origin: Origin::Drive,
                    token: token.clone(),
                    id,
                    verb: "goto".into(),
                    args: serde_json::json!({ "url": url }).to_string(),
                }],
                None => vec![],
            }
        }
        Action::KeyPressed { key, ctrl, alt } => {
            let Some(selected) = state.workspace.selected.clone() else {
                return vec![];
            };
            let Some(pane) = state.workspace.panes.iter().find(|p| p.id == selected) else {
                return vec![];
            };
            match (&state.token, encode_key(&key, ctrl, alt, pane.app_cursor)) {
                (Some(token), Some(data)) => vec![Effect::call_with(
                    Origin::Drive,
                    token,
                    &selected,
                    "input",
                    serde_json::json!({ "data": data }),
                )],
                _ => vec![],
            }
        }
        Action::ChatPosted { id, text } => {
            let text = text.trim().to_string();
            if let Some(pane) = state.workspace.panes.iter_mut().find(|p| p.id == id) {
                pane.draft.clear();
            }
            match (&state.token, text.is_empty()) {
                (Some(token), false) => vec![Effect::call_with(
                    Origin::Drive,
                    token,
                    &id,
                    "post",
                    serde_json::json!({ "text": text }),
                )],
                _ => vec![],
            }
        }
        Action::TurnCancelled { id } => match &state.token {
            Some(token) => vec![Effect::call(Origin::Drive, token, &id, "cancel")],
            None => vec![],
        },
        Action::ChatConfigured { id, model, effort } => match &state.token {
            Some(token) => {
                let mut args = serde_json::Map::new();
                if let Some(model) = model {
                    args.insert("model".into(), serde_json::Value::String(model));
                }
                if let Some(effort) = effort {
                    args.insert("effort".into(), serde_json::Value::String(effort));
                }
                vec![Effect::call_with(
                    Origin::Chrome,
                    token,
                    &id,
                    "configure",
                    serde_json::Value::Object(args),
                )]
            }
            None => vec![],
        },
        Action::PaneMeasured { id, cols, rows } => {
            let Some(pane) = state.workspace.panes.iter_mut().find(|p| p.id == id) else {
                return vec![];
            };
            // The loop-breaker: ask once per size, not once per render.
            if pane.sent_size == Some((cols, rows)) || pane.kind != "tty" || pane.gone {
                return vec![];
            }
            pane.sent_size = Some((cols, rows));
            match &state.token {
                Some(token) => vec![Effect::call_with(
                    Origin::Drive,
                    token,
                    &id,
                    "resize",
                    serde_json::json!({ "cols": cols, "rows": rows }),
                )],
                None => vec![],
            }
        }
        Action::CreateRequested { kind } => match &state.token {
            // Optimistic, like a palette verb: try with nothing, and let
            // a bad_args answer open the well with the kind's own words.
            // A chat carries the catalog default so a new conversation
            // is modeled without a second trip through the well.
            Some(token) => vec![Effect::CreateInstance {
                token: token.clone(),
                project: state.workspace.current_project.clone(),
                title: String::new(),
                args: create_args_for(&kind, &state.catalog),
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
        Action::CreateAnswered { kind, status, body } => {
            #[derive(Default, serde::Deserialize)]
            struct Wire {
                #[serde(default)]
                error: String,
                #[serde(default)]
                why: String,
            }
            let wire = serde_json::from_str::<Wire>(&body).unwrap_or_default();
            if status == 400 && wire.error == "bad_args" {
                state.palette = Some(Palette {
                    query: String::new(),
                    selected: 0,
                    stage: Stage::Args {
                        target: Target::Create { kind },
                        draft: "{}".into(),
                        error: Some(wire.why),
                    },
                });
                return vec![];
            }
            if status == 200 {
                if let Ok(info) = serde_json::from_str::<InstanceInfo>(&body) {
                    state.workspace.selected = Some(info.id);
                }
            } else {
                let why = if wire.why.is_empty() {
                    wire.error
                } else {
                    wire.why
                };
                state.notice = Some(format!("new {kind}: {why}"));
            }
            relist(state)
        }
        Action::AdminAnswered { status, body } => {
            #[derive(serde::Deserialize)]
            struct Listing {
                users: Vec<AdminUser>,
            }
            match (status, serde_json::from_str::<Listing>(&body)) {
                (200, Ok(listing)) => {
                    let open = state.admin.as_ref().is_some_and(|a| a.open);
                    let minted = state.admin.take().and_then(|a| a.minted);
                    state.admin = Some(Admin {
                        open,
                        users: listing.users,
                        minted,
                        error: None,
                    });
                }
                // 403 is the ordinary answer: not the operator. Anything
                // else leaves whatever we knew alone.
                (403, _) => state.admin = None,
                _ => {}
            }
            vec![]
        }
        Action::AdminToggled => {
            if let Some(admin) = &mut state.admin {
                admin.open = !admin.open;
                if !admin.open {
                    // Closing the panel forgets the plaintext code.
                    admin.minted = None;
                }
            }
            vec![]
        }
        Action::AdminActed { user, act } => match &state.token {
            Some(token) => vec![Effect::AdminAct {
                token: token.clone(),
                user,
                act,
            }],
            None => vec![],
        },
        Action::AdminActAnswered {
            user: _,
            act,
            status,
            body,
        } => {
            if let Some(admin) = &mut state.admin {
                admin.error = None;
                match (act, status) {
                    (AdminAct::Mint, 200) => {
                        admin.minted = serde_json::from_str::<Minted>(&body).ok();
                    }
                    (_, 200) => {}
                    _ => {
                        #[derive(serde::Deserialize)]
                        struct Wire {
                            #[serde(default)]
                            why: String,
                        }
                        admin.error = Some(
                            serde_json::from_str::<Wire>(&body)
                                .ok()
                                .map(|w| w.why)
                                .filter(|w| !w.is_empty())
                                .unwrap_or_else(|| format!("refused ({status})")),
                        );
                    }
                }
            }
            // Whatever happened, the listing is the truth to re-read.
            match &state.token {
                Some(token) => vec![Effect::FetchAdmin {
                    token: token.clone(),
                }],
                None => vec![],
            }
        }
        Action::PaletteToggled => {
            state.notice = None;
            state.renaming = None;
            state.palette = match state.palette {
                Some(_) => None,
                None => Some(Palette {
                    query: String::new(),
                    selected: 0,
                    stage: Stage::List,
                }),
            };
            vec![]
        }
        Action::PaletteQueried { query } => {
            if let Some(palette) = &mut state.palette {
                palette.query = query;
                palette.selected = 0;
            }
            vec![]
        }
        Action::PaletteMoved { delta } => {
            let rows = state
                .palette
                .as_ref()
                .map(|p| palette_rows(state, &p.query).len())
                .unwrap_or(0);
            if let Some(palette) = &mut state.palette
                && rows > 0
            {
                let max = rows as i32 - 1;
                palette.selected = (palette.selected as i32 + delta).clamp(0, max) as usize;
            }
            vec![]
        }
        Action::PaletteDismissed => {
            if let Some(palette) = &mut state.palette {
                match palette.stage {
                    Stage::Args { .. } => palette.stage = Stage::List,
                    Stage::List => state.palette = None,
                }
            }
            vec![]
        }
        Action::RenameStarted { id } => {
            if state.workspace.selected.as_deref() != Some(id.as_str()) {
                return reduce_again(state, Action::Selected { id });
            }
            let draft = state
                .workspace
                .instances
                .iter()
                .find(|i| i.id == id)
                .map(|i| {
                    if i.title.is_empty() {
                        i.kind.clone()
                    } else {
                        i.title.clone()
                    }
                })
                .unwrap_or_default();
            state.renaming = Some(RenameEdit {
                id,
                draft,
                error: None,
            });
            vec![]
        }
        Action::RenameDrafted { draft } => {
            if let Some(edit) = &mut state.renaming {
                edit.draft = draft;
                edit.error = None;
            }
            vec![]
        }
        Action::RenameDismissed => {
            state.renaming = None;
            vec![]
        }
        Action::RenameCommitted { title } => {
            let Some(edit) = &mut state.renaming else {
                return vec![];
            };
            if !valid_slug(&title) {
                edit.draft = title;
                edit.error = Some("title must match ^[A-Za-z0-9][A-Za-z0-9-]*$".into());
                return vec![];
            }
            let id = edit.id.clone();
            state.renaming = None;
            match &state.token {
                Some(token) => vec![Effect::call_with(
                    Origin::Chrome,
                    token,
                    &id,
                    "sys.rename",
                    serde_json::json!({ "title": title }),
                )],
                None => vec![],
            }
        }
        Action::PaletteCommitted => {
            let Some(palette) = &state.palette else {
                return vec![];
            };
            let rows = palette_rows(state, &palette.query);
            let Some(row) = rows.get(palette.selected) else {
                return vec![];
            };
            if let Some(reason) = &row.gated {
                state.notice = Some(format!("gated: {reason}"));
                return vec![];
            }
            let commit = row.commit.clone();
            state.palette = None;
            match commit {
                Commit::Jump { id } => reduce_again(state, Action::Selected { id }),
                Commit::Create { kind } => reduce_again(state, Action::CreateRequested { kind }),
                Commit::ClosePane { id } => reduce_again(state, Action::PaneClosed { id }),
                Commit::SignOut => reduce_again(state, Action::SignOutRequested),
                Commit::AdminPanel => reduce_again(state, Action::AdminToggled),
                Commit::Verb { id, verb } => match &state.token {
                    // Optimistic: run with null. bad_args reopens as the
                    // well, carrying the server's own complaint.
                    Some(token) => vec![Effect::call(Origin::Palette, token, &id, &verb)],
                    None => vec![],
                },
            }
        }
        Action::PaletteArgsCommitted { draft } => {
            let Some(palette) = &mut state.palette else {
                return vec![];
            };
            let Stage::Args { target, .. } = &palette.stage else {
                return vec![];
            };
            let target = target.clone();
            match serde_json::from_str::<serde_json::Value>(&draft) {
                Ok(_) => {
                    state.palette = None;
                    match &state.token {
                        Some(token) => vec![match target {
                            Target::Verb { id, verb } => Effect::CallVerb {
                                origin: Origin::Palette,
                                token: token.clone(),
                                id,
                                verb,
                                args: draft,
                            },
                            Target::Create { kind } => Effect::CreateInstance {
                                token: token.clone(),
                                project: state.workspace.current_project.clone(),
                                title: String::new(),
                                kind,
                                args: draft,
                            },
                        }],
                        None => vec![],
                    }
                }
                Err(e) => {
                    if let Some(palette) = &mut state.palette
                        && let Stage::Args {
                            draft: d, error, ..
                        } = &mut palette.stage
                    {
                        *d = draft;
                        *error = Some(format!("that is not JSON: {e}"));
                    }
                    vec![]
                }
            }
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
                title: String::new(),
                args: "{}".into(),
            }]
        );
        reduce(
            &mut state,
            Action::CreateAnswered {
                kind: "tty".into(),
                status: 200,
                body: r#"{"id":"new-1","kind":"tty","project":"",
                          "title":"tty","creator":{"kind":"human","id":"ada"}}"#
                    .into(),
            },
        );
        assert_eq!(state.workspace.selected.as_deref(), Some("new-1"));
    }

    /// "new host" from the palette: the optimistic create meets bad_args,
    /// the well opens targeted at the kind with the server's own words,
    /// and committing the well creates with the drafted args.
    #[test]
    fn a_refused_create_opens_the_well_and_the_well_creates() {
        let mut state = signed_in();
        let effects = reduce(
            &mut state,
            Action::CreateRequested {
                kind: "host".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::CreateInstance {
                token: "tok-1".into(),
                kind: "host".into(),
                project: String::new(),
                title: String::new(),
                args: "{}".into(),
            }]
        );

        let effects = reduce(
            &mut state,
            Action::CreateAnswered {
                kind: "host".into(),
                status: 400,
                body: r#"{"error":"bad_args","why":"a host needs {command}"}"#.into(),
            },
        );
        assert!(
            effects.is_empty(),
            "the refusal opens the well, not a fetch"
        );
        let palette = state.palette.as_ref().expect("the well opened");
        let Stage::Args { target, error, .. } = &palette.stage else {
            panic!("expected the args stage, got {:?}", palette.stage);
        };
        assert_eq!(
            *target,
            Target::Create {
                kind: "host".into()
            }
        );
        assert_eq!(error.as_deref(), Some("a host needs {command}"));

        let effects = reduce(
            &mut state,
            Action::PaletteArgsCommitted {
                draft: r#"{"command":"ssh box myco-hostd"}"#.into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::CreateInstance {
                token: "tok-1".into(),
                kind: "host".into(),
                project: String::new(),
                title: String::new(),
                args: r#"{"command":"ssh box myco-hostd"}"#.into(),
            }]
        );
        assert!(state.palette.is_none(), "the well closes on commit");
    }

    /// The push dance starts from the owner's notifier and lands its
    /// answer as a sentence under the button.
    #[test]
    fn enabling_push_finds_the_owners_notifier() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: r#"[
                    {"id":"n-other","kind":"notifier","project":"","title":"",
                     "creator":{"kind":"human","id":"grace"}},
                    {"id":"n-1","kind":"notifier","project":"","title":"",
                     "creator":{"kind":"human","id":"ada"}}
                ]"#
                .into(),
            },
        );
        let effects = reduce(&mut state, Action::EnablePushRequested);
        assert_eq!(
            effects,
            vec![Effect::EnablePush {
                token: "tok-1".into(),
                notifier: "n-1".into(),
            }],
            "the dance starts from MY notifier, not the first one listed"
        );
        reduce(
            &mut state,
            Action::PushEnrollAnswered {
                note: "notifications on".into(),
            },
        );
        assert_eq!(state.push_note.as_deref(), Some("notifications on"));
    }

    /// The URL row drives: a committed nav becomes a goto with a scheme
    /// filled in, and an empty row commits nothing.
    #[test]
    fn nav_commits_a_goto_with_a_scheme() {
        let mut state = signed_in();
        let effects = reduce(
            &mut state,
            Action::NavCommitted {
                id: "b-1".into(),
                url: "  example.com  ".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::CallVerb {
                origin: Origin::Drive,
                token: "tok-1".into(),
                id: "b-1".into(),
                verb: "goto".into(),
                args: r#"{"url":"https://example.com"}"#.into(),
            }]
        );

        let effects = reduce(
            &mut state,
            Action::NavCommitted {
                id: "b-1".into(),
                url: "data:text/html,hi".into(),
            },
        );
        assert!(matches!(
            &effects[..],
            [Effect::CallVerb { args, .. }] if args.contains("data:text/html")
        ));

        assert!(
            reduce(
                &mut state,
                Action::NavCommitted {
                    id: "b-1".into(),
                    url: "   ".into(),
                }
            )
            .is_empty()
        );
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
                title: String::new(),
                args: "{}".into(),
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

    #[test]
    fn the_key_encoder_speaks_terminal() {
        assert_eq!(encode_key("a", false, false, false).unwrap(), "a");
        assert_eq!(encode_key("Enter", false, false, false).unwrap(), "\r");
        assert_eq!(
            encode_key("Backspace", false, false, false).unwrap(),
            "\x7f"
        );
        assert_eq!(encode_key("c", true, false, false).unwrap(), "\x03");
        assert_eq!(
            encode_key("ArrowUp", false, false, false).unwrap(),
            "\x1b[A"
        );
        assert_eq!(
            encode_key("ArrowUp", false, false, true).unwrap(),
            "\x1bOA",
            "DECCKM: application cursor sends SS3"
        );
        assert_eq!(
            encode_key("b", false, true, false).unwrap(),
            "\x1bb",
            "alt is ESC"
        );
        assert_eq!(
            encode_key("Shift", false, false, false),
            None,
            "bare modifiers stay home"
        );
    }

    #[test]
    fn keys_go_to_the_terminal_only_from_its_driver() {
        let mut state = with_tty(signed_in());
        reduce(&mut state, Action::Selected { id: "t1".into() });
        assert!(wants_key(&state, "a", false, false), "driver, focused tty");
        assert!(
            !wants_key(&state, "p", true, false),
            "the palette chord is the room's"
        );

        // Someone else takes the seat: the keyboard goes quiet.
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: r#"[{"id":"t1","kind":"tty","project":"","title":"shell",
                           "creator":{"kind":"human","id":"ada"},
                           "driver":{"kind":"agent","id":"c9"}}]"#
                    .into(),
            },
        );
        assert!(!wants_key(&state, "a", false, false), "not the driver");

        let effects = reduce(
            &mut state,
            Action::KeyPressed {
                key: "Enter".into(),
                ctrl: false,
                alt: false,
            },
        );
        assert_eq!(
            effects,
            vec![Effect::call_with(
                Origin::Drive,
                "tok-1",
                "t1",
                "input",
                serde_json::json!({ "data": "\r" })
            )],
            "the reducer encodes; the edge only carried the event"
        );
    }

    #[test]
    fn measuring_asks_for_a_resize_exactly_once_per_size() {
        let mut state = with_tty(signed_in());
        reduce(&mut state, Action::Selected { id: "t1".into() });
        let effects = reduce(
            &mut state,
            Action::PaneMeasured {
                id: "t1".into(),
                cols: 120,
                rows: 32,
            },
        );
        assert_eq!(
            effects,
            vec![Effect::call_with(
                Origin::Drive,
                "tok-1",
                "t1",
                "resize",
                serde_json::json!({ "cols": 120, "rows": 32 })
            )]
        );
        // The same measurement again is a no-op — the measure/resize loop
        // cannot spin.
        let effects = reduce(
            &mut state,
            Action::PaneMeasured {
                id: "t1".into(),
                cols: 120,
                rows: 32,
            },
        );
        assert!(effects.is_empty());
    }

    /// A keystroke's answer must not re-list; a seat verb's must. The
    /// origin says which, so nobody has to test a verb name for a prefix.
    #[test]
    fn only_the_chrome_origin_moves_the_listing() {
        let mut state = with_tty(signed_in());
        let replied = |origin, verb: &str| Action::VerbReplied {
            origin,
            id: "t1".into(),
            verb: verb.into(),
            status: 200,
            body: String::new(),
        };
        assert!(
            reduce(&mut state, replied(Origin::Drive, "input")).is_empty(),
            "a keystroke costs no listing"
        );
        assert!(!reduce(&mut state, replied(Origin::Chrome, "sys.take")).is_empty());
    }

    /// A tty workspace with verbs in the kind spec, pane open and focused.
    fn with_palette_world() -> State {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::KindsAnswered {
                status: 200,
                body: r#"[{"kind":"tty","doc":"a terminal","primary_render":"screen",
                    "verbs":[
                      {"name":"input","doc":"type","requires_driver":true},
                      {"name":"screen","doc":"look","read_only":true},
                      {"name":"signal","doc":"send a signal","requires_driver":true}
                    ]}]"#
                    .into(),
            },
        );
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: r#"[{"id":"t1","kind":"tty","project":"","title":"shell",
                           "creator":{"kind":"human","id":"ada"},
                           "driver":{"kind":"agent","id":"c9"}}]"#
                    .into(),
            },
        );
        reduce(&mut state, Action::Selected { id: "t1".into() });
        state
    }

    #[test]
    fn the_registry_derives_verbs_and_gates_them_by_seat() {
        let state = with_palette_world();
        let rows = palette_rows(&state, "");
        let input = rows.iter().find(|r| r.label == "input").expect("input row");
        assert_eq!(
            input.gated.as_deref(),
            Some("agent driving"),
            "gated rows teach the seat"
        );
        let screen = rows
            .iter()
            .find(|r| r.label == "screen")
            .expect("screen row");
        assert!(screen.gated.is_none(), "reads are never gated");
        assert!(rows.iter().any(|r| r.label == "open shell"));
        assert!(rows.iter().any(|r| r.label == "new tty"));
        let rename = rows
            .iter()
            .find(|r| r.label == "rename")
            .expect("sys.rename is injected");
        assert_eq!(
            rename.commit,
            Commit::Verb {
                id: "t1".into(),
                verb: "sys.rename".into()
            }
        );
        assert!(rename.gated.is_none());

        // The fuzzy filter is a subsequence match.
        let rows = palette_rows(&state, "sgn");
        assert!(rows.iter().any(|r| r.label == "signal"));
        assert!(!rows.iter().any(|r| r.label == "screen"));
    }

    #[test]
    fn committing_a_verb_runs_optimistically_and_bad_args_opens_the_well() {
        let mut state = with_palette_world();
        reduce(&mut state, Action::PaletteToggled);
        reduce(
            &mut state,
            Action::PaletteQueried {
                query: "screen".into(),
            },
        );
        let effects = reduce(&mut state, Action::PaletteCommitted);
        assert_eq!(
            effects,
            vec![Effect::call(Origin::Palette, "tok-1", "t1", "screen")]
        );
        assert!(state.palette.is_none(), "commit closes the list");

        // The server wanted args: the well opens with its words.
        reduce(
            &mut state,
            Action::VerbReplied {
                origin: Origin::Palette,
                id: "t1".into(),
                verb: "screen".into(),
                status: 400,
                body: r#"{"error":"bad_args","why":"needs {from}"}"#.into(),
            },
        );
        let palette = state.palette.as_ref().expect("the well is open");
        match &palette.stage {
            Stage::Args { target, error, .. } => {
                assert_eq!(target.label(), "screen");
                assert_eq!(error.as_deref(), Some("needs {from}"));
            }
            other => panic!("expected the args stage, got {other:?}"),
        }

        // Bad JSON stays in the well with a local complaint; good JSON runs.
        reduce(
            &mut state,
            Action::PaletteArgsCommitted {
                draft: "not json".into(),
            },
        );
        assert!(state.palette.is_some());
        let effects = reduce(
            &mut state,
            Action::PaletteArgsCommitted {
                draft: r#"{"from": 0}"#.into(),
            },
        );
        assert!(matches!(
            &effects[..],
            [Effect::CallVerb {
                origin: Origin::Palette,
                args,
                ..
            }] if args.contains("from")
        ));
        assert!(state.palette.is_none());
    }

    #[test]
    fn a_gated_commit_refuses_with_the_reason_and_a_result_lands_as_notice() {
        let mut state = with_palette_world();
        reduce(&mut state, Action::PaletteToggled);
        reduce(
            &mut state,
            Action::PaletteQueried {
                query: "input".into(),
            },
        );
        let effects = reduce(&mut state, Action::PaletteCommitted);
        assert!(effects.is_empty());
        assert!(state.notice.as_deref().unwrap().contains("agent driving"));
        assert!(
            state.palette.is_some(),
            "a gated row does not close the palette"
        );

        reduce(
            &mut state,
            Action::VerbReplied {
                origin: Origin::Palette,
                id: "t1".into(),
                verb: "text".into(),
                status: 200,
                body: r#"{"text":"hi"}"#.into(),
            },
        );
        assert!(state.notice.as_deref().unwrap().contains("hi"));
    }

    #[test]
    fn the_palette_owns_the_keyboard_while_open() {
        let mut state = with_palette_world();
        // Make ada the driver so keys would otherwise flow.
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
        assert!(wants_key(&state, "a", false, false));
        reduce(&mut state, Action::PaletteToggled);
        assert!(!wants_key(&state, "a", false, false));
        // Escape closes; the terminal gets its keys back.
        reduce(&mut state, Action::PaletteDismissed);
        assert!(wants_key(&state, "a", false, false));
    }

    const LISTING: &str = r#"{"operator":"ada","users":[
        {"id":"ada","name":"Ada","disabled":false,"sessions":1,"passkeys":1,"operator":true},
        {"id":"grace","name":"Grace","disabled":false,"sessions":2,"passkeys":0,"operator":false}
    ]}"#;

    #[test]
    fn the_admin_surface_exists_exactly_when_the_server_says_operator() {
        let mut state = signed_in();
        // Entering the workspace probed the admin listing.
        reduce(
            &mut state,
            Action::AdminAnswered {
                status: 200,
                body: LISTING.into(),
            },
        );
        let admin = state.admin.as_ref().expect("operator");
        assert_eq!(admin.users.len(), 2);
        assert!(
            palette_rows(&state, "admin")
                .iter()
                .any(|r| r.label == "admin panel")
        );

        // A non-operator's 403 is the answer, not an error.
        reduce(
            &mut state,
            Action::AdminAnswered {
                status: 403,
                body: r#"{"error":"forbidden","why":"operator only"}"#.into(),
            },
        );
        assert!(state.admin.is_none());
        assert!(
            !palette_rows(&state, "admin")
                .iter()
                .any(|r| r.label == "admin panel")
        );
    }

    #[test]
    fn minting_shows_the_code_once_and_closing_forgets_it() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::AdminAnswered {
                status: 200,
                body: LISTING.into(),
            },
        );
        reduce(&mut state, Action::AdminToggled);
        let effects = reduce(
            &mut state,
            Action::AdminActed {
                user: "grace".into(),
                act: AdminAct::Mint,
            },
        );
        assert_eq!(
            effects,
            vec![Effect::AdminAct {
                token: "tok-1".into(),
                user: "grace".into(),
                act: AdminAct::Mint,
            }]
        );
        let effects = reduce(
            &mut state,
            Action::AdminActAnswered {
                user: "grace".into(),
                act: AdminAct::Mint,
                status: 200,
                body: r#"{"username":"grace","code":"AAAAA-BBBBB","expires_at":"soon"}"#.into(),
            },
        );
        assert!(
            effects.contains(&Effect::FetchAdmin {
                token: "tok-1".into()
            }),
            "every act re-reads the listing"
        );
        assert_eq!(
            state.admin.as_ref().unwrap().minted.as_ref().unwrap().code,
            "AAAAA-BBBBB"
        );
        // The re-read keeps the minted code on screen…
        reduce(
            &mut state,
            Action::AdminAnswered {
                status: 200,
                body: LISTING.into(),
            },
        );
        assert!(state.admin.as_ref().unwrap().minted.is_some());
        // …and closing the panel forgets the plaintext.
        reduce(&mut state, Action::AdminToggled);
        assert!(state.admin.as_ref().unwrap().minted.is_none());
    }

    #[test]
    fn a_refused_act_lands_in_the_servers_words() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::AdminAnswered {
                status: 200,
                body: LISTING.into(),
            },
        );
        reduce(
            &mut state,
            Action::AdminActAnswered {
                user: "ada".into(),
                act: AdminAct::Disable,
                status: 400,
                body: r#"{"error":"refused","why":"the operator account cannot be disabled or removed"}"#.into(),
            },
        );
        assert!(
            state
                .admin
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("cannot be disabled")
        );
    }

    #[test]
    fn a_chat_post_sends_the_trimmed_text_and_clears_the_draft() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::KindsAnswered {
                status: 200,
                body: r#"[{"kind":"chat","doc":"","primary_render":"tail"}]"#.into(),
            },
        );
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: r#"[{"id":"c1","kind":"chat","project":"","title":"chat",
                           "creator":{"kind":"human","id":"ada"},"driver":null}]"#
                    .into(),
            },
        );
        let effects = reduce(&mut state, Action::Selected { id: "c1".into() });
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::CallVerb { verb, .. } if verb == "about"
            )),
            "opening a chat reads about so the dials have a value: {effects:?}"
        );

        let effects = reduce(
            &mut state,
            Action::ChatPosted {
                id: "c1".into(),
                text: "  hello agent  ".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::call_with(
                Origin::Drive,
                "tok-1",
                "c1",
                "post",
                serde_json::json!({ "text": "hello agent" })
            )]
        );
        assert!(state.workspace.panes[0].draft.is_empty());

        // Empty (or whitespace) posts send nothing.
        let effects = reduce(
            &mut state,
            Action::ChatPosted {
                id: "c1".into(),
                text: "   ".into(),
            },
        );
        assert!(effects.is_empty());

        // Cancel maps to the chat's cancel verb.
        let effects = reduce(&mut state, Action::TurnCancelled { id: "c1".into() });
        assert_eq!(
            effects,
            vec![Effect::call(Origin::Drive, "tok-1", "c1", "cancel")]
        );
    }

    #[test]
    fn a_new_chat_carries_the_catalog_default() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::ModelsAnswered {
                status: 200,
                body: r#"{"default":"grok-4-6","models":[
                    {"key":"grok-4-6","ready":true,"effort":"high","default":true},
                    {"key":"claude-opus-5","ready":true,"effort":"high","default":false}
                ]}"#
                .into(),
            },
        );
        let effects = reduce(
            &mut state,
            Action::CreateRequested {
                kind: "chat".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::CreateInstance {
                token: "tok-1".into(),
                kind: "chat".into(),
                project: String::new(),
                title: String::new(),
                args: r#"{"model":"grok-4-6"}"#.into(),
            }]
        );
        // A tty is still empty-args — the catalog is not its business.
        let effects = reduce(&mut state, Action::CreateRequested { kind: "tty".into() });
        assert_eq!(
            effects,
            vec![Effect::CreateInstance {
                token: "tok-1".into(),
                kind: "tty".into(),
                project: String::new(),
                title: String::new(),
                args: "{}".into(),
            }]
        );
    }

    #[test]
    fn configuring_a_chat_sends_only_the_dials_that_moved() {
        let mut state = signed_in();
        reduce(
            &mut state,
            Action::KindsAnswered {
                status: 200,
                body: r#"[{"kind":"chat","doc":"","primary_render":"tail"}]"#.into(),
            },
        );
        reduce(
            &mut state,
            Action::InstancesAnswered {
                status: 200,
                body: r#"[{"id":"c1","kind":"chat","project":"","title":"chat",
                           "creator":{"kind":"human","id":"ada"},"driver":null}]"#
                    .into(),
            },
        );
        reduce(&mut state, Action::Selected { id: "c1".into() });
        let effects = reduce(
            &mut state,
            Action::ChatConfigured {
                id: "c1".into(),
                model: Some("claude-opus-5".into()),
                effort: None,
            },
        );
        assert_eq!(
            effects,
            vec![Effect::call_with(
                Origin::Chrome,
                "tok-1",
                "c1",
                "configure",
                serde_json::json!({ "model": "claude-opus-5" })
            )]
        );
        reduce(
            &mut state,
            Action::VerbReplied {
                origin: Origin::Chrome,
                id: "c1".into(),
                verb: "configure".into(),
                status: 200,
                body: r#"{"model":"claude-opus-5","effort":"high","len":0,"turn_running":false,"watching":[]}"#.into(),
            },
        );
        assert_eq!(
            state.workspace.panes[0]
                .about
                .as_ref()
                .map(|a| a.model.as_deref()),
            Some(Some("claude-opus-5"))
        );
    }

    /// Selecting a pane that is already open and selected moves nothing
    /// the renderer reads. That is what lets the edge skip the region —
    /// and what lets a caret in that pane's composer survive the click
    /// that put it there.
    #[test]
    fn reselecting_an_open_pane_leaves_the_rendered_state_alone() {
        let mut state = with_tty(signed_in());
        reduce(&mut state, Action::Selected { id: "t1".into() });
        let before = state.clone();

        let effects = reduce(&mut state, Action::Selected { id: "t1".into() });
        assert!(effects.is_empty(), "no re-watch, no re-read");

        // The action log is the one thing that moves, and nothing renders it.
        let mut after = state.clone();
        after.log = before.log.clone();
        assert_eq!(after, before);
    }

    #[test]
    fn inline_rename_refuses_a_bad_slug_and_calls_chrome() {
        let mut state = with_palette_world();
        reduce(&mut state, Action::RenameStarted { id: "t1".into() });
        assert_eq!(
            state.renaming.as_ref().map(|r| r.draft.as_str()),
            Some("shell")
        );
        assert!(!wants_key(&state, "a", false, false));

        let effects = reduce(
            &mut state,
            Action::RenameCommitted {
                title: "has space".into(),
            },
        );
        assert!(effects.is_empty());
        assert!(
            state
                .renaming
                .as_ref()
                .and_then(|r| r.error.as_ref())
                .is_some()
        );

        let effects = reduce(
            &mut state,
            Action::RenameCommitted {
                title: "lab-1".into(),
            },
        );
        assert_eq!(
            effects,
            vec![Effect::call_with(
                Origin::Chrome,
                "tok-1",
                "t1",
                "sys.rename",
                serde_json::json!({ "title": "lab-1" })
            )]
        );
        assert!(state.renaming.is_none());
    }

    #[test]
    fn a_palette_rename_is_chrome_not_a_notice() {
        let mut state = with_palette_world();
        reduce(
            &mut state,
            Action::VerbReplied {
                origin: Origin::Palette,
                id: "t1".into(),
                verb: "sys.rename".into(),
                status: 200,
                body: "null".into(),
            },
        );
        assert!(state.notice.is_none(), "rename must not dump JSON");
        assert!(state.listing_in_flight, "chrome-like: re-list");
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
