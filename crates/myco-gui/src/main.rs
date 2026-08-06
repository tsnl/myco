//! Minimal Yew client for the myco API server: a session browser at `/` and
//! one conversation per URL at `/session/<id>`. Live output arrives over SSE;
//! a slow poll reconciles the transcript as a fallback.
//!
//! The visual identity is the terminal's: monospace, dark, USER/ASSISTANT
//! section rules — the web page renders (approximately) what the CLI prints,
//! with no further chrome.

mod auth;
mod highlight;
mod state;
mod transcript;
mod work;

use transcript::*;
use work::*;

use std::cell::RefCell;
use std::rc::Rc;

use state::{ConvAction, ConvState, StreamItem};

use futures::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use myco_api as api;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement};
use yew::prelude::*;
use yew_router::prelude::*;

use auth::Failure;

/// Who we are signed in as, and how to sign out. Provided by [`App`] once the
/// token has been checked, so nothing below it renders un-authenticated.
#[derive(Clone, PartialEq)]
struct Session {
    identity: api::Identity,
    sign_out: Callback<()>,
}

fn sign_out_now(ctx: &Option<Session>) {
    if let Some(s) = ctx {
        s.sign_out.emit(());
    }
}

#[derive(Clone, Routable, PartialEq)]
enum Route {
    #[at("/")]
    Browser,
    /// A conversation that does not exist yet: the session is created by the
    /// first message, so opening a new one and walking away costs nothing.
    #[at("/new")]
    Draft,
    #[at("/session/:id")]
    Session { id: String },
}

fn main() {
    yew::Renderer::<App>::new().render();
}

#[function_component(App)]
fn app() -> Html {
    // `None` = not signed in (or not checked yet); the router is not rendered
    // until a token has actually been accepted by the server.
    let identity = use_state(|| Option::<api::Identity>::None);
    let checked = use_state(|| false);

    {
        let identity = identity.clone();
        let checked = checked.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                if let Ok(who) = auth::whoami().await {
                    identity.set(Some(who));
                }
                checked.set(true);
            });
        });
    }

    let on_signed_in = {
        let identity = identity.clone();
        Callback::from(move |who: api::Identity| identity.set(Some(who)))
    };
    let sign_out = {
        let identity = identity.clone();
        Callback::from(move |_: ()| {
            let identity = identity.clone();
            spawn_local(async move {
                auth::logout().await;
                identity.set(None);
            });
        })
    };

    if !*checked {
        return html! { <div class="app"><div class="pane"><div class="column">
            <span class="dim">{ "…" }</span>
        </div></div></div> };
    }
    let Some(who) = (*identity).clone() else {
        return html! { <Login {on_signed_in} /> };
    };
    let session = Session {
        identity: who,
        sign_out,
    };

    html! {
        <ContextProvider<Session> context={session}>
            <BrowserRouter>
                <Switch<Route> render={|route| match route {
                    Route::Browser => html! { <Browser /> },
                    Route::Draft => html! { <Draft /> },
                    Route::Session { id } => html! { <Conversation {id} /> },
                }} />
            </BrowserRouter>
        </ContextProvider<Session>>
    }
}

#[derive(Properties, PartialEq)]
struct LoginProps {
    on_signed_in: Callback<api::Identity>,
}

/// Sign-in. Deliberately the whole page and nothing else: there is no
/// anonymous view of this server to decorate, and no signup — accounts are
/// created by an administrator with `myco auth`.
#[function_component(Login)]
fn login(props: &LoginProps) -> Html {
    let username = use_node_ref();
    let password = use_node_ref();
    let error = use_state(|| Option::<String>::None);
    let busy = use_state(|| false);

    let submit: Callback<()> = {
        let username = username.clone();
        let password = password.clone();
        let error = error.clone();
        let busy = busy.clone();
        let on_signed_in = props.on_signed_in.clone();
        Callback::from(move |_| {
            let (Some(u), Some(p)) = (
                username.cast::<HtmlInputElement>(),
                password.cast::<HtmlInputElement>(),
            ) else {
                return;
            };
            let (u, p) = (u.value(), p.value());
            if u.trim().is_empty() || p.is_empty() {
                error.set(Some("username and password are required".into()));
                return;
            }
            busy.set(true);
            let error = error.clone();
            let busy = busy.clone();
            let on_signed_in = on_signed_in.clone();
            spawn_local(async move {
                match auth::login(&u, &p).await {
                    Ok(who) => {
                        error.set(None);
                        busy.set(false);
                        on_signed_in.emit(who);
                    }
                    Err(Failure::Unauthorized) => {
                        busy.set(false);
                        // The server does not say which half was wrong, and
                        // neither do we.
                        error.set(Some("incorrect username or password".into()));
                    }
                    Err(e) => {
                        busy.set(false);
                        error.set(Some(e.to_string()));
                    }
                }
            });
        })
    };
    let on_click = to_click(&submit);
    let on_keydown = {
        let submit = submit.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                submit.emit(());
            }
        })
    };

    // A passkey needs only the username; the authenticator is the secret.
    let on_passkey = {
        let username = username.clone();
        let error = error.clone();
        let busy = busy.clone();
        let on_signed_in = props.on_signed_in.clone();
        Callback::from(move |_: MouseEvent| {
            let Some(u) = username.cast::<HtmlInputElement>() else {
                return;
            };
            let u = u.value();
            if u.trim().is_empty() {
                error.set(Some("enter your username, then use your passkey".into()));
                return;
            }
            busy.set(true);
            let error = error.clone();
            let busy = busy.clone();
            let on_signed_in = on_signed_in.clone();
            spawn_local(async move {
                match auth::passkey_login(&u).await {
                    Ok(who) => {
                        error.set(None);
                        busy.set(false);
                        on_signed_in.emit(who);
                    }
                    Err(e) => {
                        busy.set(false);
                        error.set(Some(e.to_string()));
                    }
                }
            });
        })
    };

    // A one-time code rides the password field: same box, different grant.
    let on_code = {
        let username = username.clone();
        let password = password.clone();
        let error = error.clone();
        let busy = busy.clone();
        let on_signed_in = props.on_signed_in.clone();
        Callback::from(move |_: MouseEvent| {
            let (Some(u), Some(c)) = (
                username.cast::<HtmlInputElement>(),
                password.cast::<HtmlInputElement>(),
            ) else {
                return;
            };
            let (u, c) = (u.value(), c.value());
            if u.trim().is_empty() || c.trim().is_empty() {
                error.set(Some(
                    "enter your username and the one-time code (in the password box)".into(),
                ));
                return;
            }
            busy.set(true);
            let error = error.clone();
            let busy = busy.clone();
            let on_signed_in = on_signed_in.clone();
            spawn_local(async move {
                match auth::code_login(&u, &c).await {
                    Ok(who) => {
                        error.set(None);
                        busy.set(false);
                        on_signed_in.emit(who);
                    }
                    Err(e) => {
                        busy.set(false);
                        error.set(Some(e.to_string()));
                    }
                }
            });
        })
    };

    html! {
        <div class="app">
            <div class="pane">
                <div class="column login">
                    <h1>{ "myco" }</h1>
                    <div class="login-row">
                        <input ref={username} type="text" autocomplete="username webauthn"
                               onkeydown={on_keydown.clone()} placeholder="username" />
                    </div>
                    <div class="login-row">
                        <input ref={password} type="password" autocomplete="current-password"
                               onkeydown={on_keydown} placeholder="password or one-time code" />
                    </div>
                    <div class="login-row">
                        <button onclick={on_click} disabled={*busy}>
                            { if *busy { "signing in…" } else { "sign in" } }
                        </button>
                        { " " }
                        <button onclick={on_passkey} disabled={*busy}>{ "passkey" }</button>
                        { " " }
                        <button onclick={on_code} disabled={*busy}
                                title="redeem a one-time code from the operator (typed in the password box)">
                            { "use code" }
                        </button>
                    </div>
                    { if let Some(e) = &*error { html! {
                        <div class="err"><pre>{ e }</pre></div>
                    } } else { html!{} } }
                </div>
            </div>
        </div>
    }
}

/// `41.2k` — tokens at a glance; exact numbers belong in tooltips.
fn fmt_tokens(n: u64) -> String {
    if n >= 10_000 {
        format!("{}k", n / 1_000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// `signed in as <name>`, with the sign-out and passkey affordances next to
/// it. Enrollment lives here — any authenticated session may add a passkey
/// for its own account, which is how a password (or one-time code) bootstrap
/// graduates to phishing-proof sign-in.
fn whoami_line(who: &Option<Session>) -> Html {
    let Some(s) = who else { return html! {} };
    let sign_out = {
        let cb = s.sign_out.clone();
        Callback::from(move |_: MouseEvent| cb.emit(()))
    };
    let add_passkey = Callback::from(move |_: MouseEvent| {
        spawn_local(async move {
            let message = match auth::register_passkey().await {
                Ok(()) => "passkey enrolled — you can sign in with it from now on".to_string(),
                Err(e) => e.to_string(),
            };
            if let Some(w) = web_sys::window() {
                let _ = w.alert_with_message(&message);
            }
        });
    });
    html! {
        <span class="dim">
            { format!(" · {} ", s.identity.name) }
            <button class="linkish" onclick={add_passkey}
                    title="enroll this device's passkey for your account">
                { "add passkey" }
            </button>
            { " " }
            <button class="linkish" onclick={sign_out}>{ "sign out" }</button>
        </span>
    }
}

/// Click and Enter both mean "send": one action, two bindings. Enter submits
/// and Shift+Enter inserts a newline, as in the terminal.
fn to_click(send: &Callback<()>) -> Callback<MouseEvent> {
    let send = send.clone();
    Callback::from(move |_: MouseEvent| send.emit(()))
}

fn to_enter(send: &Callback<()>) -> Callback<KeyboardEvent> {
    let send = send.clone();
    Callback::from(move |e: KeyboardEvent| {
        if e.key() == "Enter" && !e.shift_key() {
            e.prevent_default();
            send.emit(());
        }
    })
}

async fn fetch_detail(id: &str) -> Result<api::SessionDetail, Failure> {
    auth::get(&format!("/api/sessions/{id}")).await
}

async fn fetch_poll(id: &str) -> Result<api::Poll, Failure> {
    auth::get(&format!("/api/sessions/{id}/poll?since=0")).await
}

// ---------------------------------------------------------------------------
// Session browser (top level)
// ---------------------------------------------------------------------------

#[function_component(Browser)]
fn browser() -> Html {
    let who = use_context::<Session>();
    let sessions = use_state(Vec::<api::SessionSummary>::new);
    let show_archived = use_state(|| false);
    let reload = use_state(|| 0_u32);
    let navigator = use_navigator().unwrap();

    {
        let sessions = sessions.clone();
        let who = who.clone();
        use_effect_with((*show_archived, *reload), move |(archived, _)| {
            let archived = *archived;
            let who = who.clone();
            spawn_local(async move {
                let url = format!("/api/sessions?include_archived={archived}");
                match auth::get::<Vec<api::SessionSummary>>(&url).await {
                    Ok(list) => sessions.set(list),
                    Err(Failure::Unauthorized) => sign_out_now(&who),
                    Err(_) => {}
                }
            });
        });
    }

    let toggle_archived = {
        let show_archived = show_archived.clone();
        Callback::from(move |_: MouseEvent| show_archived.set(!*show_archived))
    };

    let on_new = {
        let navigator = navigator.clone();
        Callback::from(move |_| navigator.push(&Route::Draft))
    };

    html! {
        <div class="browser">
            <div class="column">
            <h1>{ "myco" }</h1>
            { whoami_line(&who) }
            <button onclick={on_new}>{ "new session" }</button>
            { " " }
            <button onclick={toggle_archived}>
                { if *show_archived { "hide archived" } else { "show archived" } }
            </button>
            <ul>
                { for sessions.iter().map(|s| {
                    let title = s.title.clone().unwrap_or_else(|| s.snippet.clone());
                    let label = format!(
                        "{} — {} [{}{}{}] {}",
                        &s.id[..s.id.len().min(8)],
                        s.model,
                        if s.live { "live" } else { "idle" },
                        if s.busy { ", busy" } else { "" },
                        if s.archived { ", archived" } else { "" },
                        title,
                    );
                    let on_archive = {
                        let id = s.id.clone();
                        let archived = s.archived;
                        let reload = reload.clone();
                        Callback::from(move |_: MouseEvent| {
                            let id = id.clone();
                            let reload = reload.clone();
                            spawn_local(async move {
                                let _ = auth::patch_json::<api::SessionSummary, _>(
                                    &format!("/api/sessions/{id}"),
                                    &api::UpdateSession {
                                        archived: Some(!archived),
                                        ..Default::default()
                                    },
                                )
                                .await;
                                reload.set(*reload + 1);
                            });
                        })
                    };
                    html! {
                        <li>
                            <Link<Route> to={Route::Session { id: s.id.clone() }}>{ label }</Link<Route>>
                            { " " }
                            <button onclick={on_archive}>
                                { if s.archived { "unarchive" } else { "archive" } }
                            </button>
                        </li>
                    }
                }) }
            </ul>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Draft (a conversation before its first message)
// ---------------------------------------------------------------------------

#[function_component(Draft)]
fn draft() -> Html {
    let input = use_node_ref();
    let error = use_state(|| Option::<String>::None);
    let navigator = use_navigator().unwrap();
    // The catalog, so a session can start on a chosen model rather than being
    // switched right after its first turn.
    let models = use_state(|| Option::<api::Models>::None);
    let picked = use_node_ref();
    {
        let models = models.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                if let Ok(m) = auth::get::<api::Models>("/api/models").await {
                    models.set(Some(m));
                }
            });
        });
    }

    let send_now: Callback<()> = {
        let input = input.clone();
        let picked = picked.clone();
        let error = error.clone();
        let navigator = navigator.clone();
        Callback::from(move |_| {
            let Some(ta) = input.cast::<HtmlTextAreaElement>() else {
                return;
            };
            let text = ta.value();
            if text.trim().is_empty() {
                return;
            }
            ta.set_value("");
            // No picker rendered (catalog not loaded) → server default.
            let model = picked
                .cast::<HtmlSelectElement>()
                .map(|s| s.value())
                .filter(|v| !v.is_empty());
            let error = error.clone();
            let navigator = navigator.clone();
            spawn_local(async move {
                // Create, then post, then hand the URL over to `Conversation`.
                let created = auth::post_json::<api::SessionSummary, _>(
                    "/api/sessions",
                    &api::CreateSession {
                        model,
                        parent_session: None,
                        fork: false,
                    },
                )
                .await;
                let summary = match created {
                    Ok(s) => s,
                    Err(e) => {
                        error.set(Some(format!("new session: {e}")));
                        return;
                    }
                };
                let posted = auth::post_json::<api::Poll, _>(
                    &format!("/api/sessions/{}/messages", summary.id),
                    &api::PostMessage { text },
                )
                .await;
                if let Err(e) = posted {
                    error.set(Some(format!("send failed: {e}")));
                    return;
                }
                navigator.replace(&Route::Session { id: summary.id });
            });
        })
    };
    let on_send = to_click(&send_now);
    let on_keydown = to_enter(&send_now);

    html! {
        <div class="app">
            <div class="topbar">
                <div class="column">
                    <Link<Route> to={Route::Browser}>{ "← sessions" }</Link<Route>>
                    <span class="dim">{ " new session" }</span>
                </div>
            </div>
            <div class="pane">
                <div class="column">
                    { if let Some(e) = &*error { html! {
                        <div class="err"><b>{ "ERROR" }</b><pre>{ e }</pre></div>
                    } } else { html!{} } }
                </div>
            </div>
            <div class="composer">
                <div class="column">
                    <textarea ref={input} rows="3" onkeydown={on_keydown}
                              placeholder="message (Enter to send, Shift+Enter for a newline)" />
                    <div class="actions">
                        { if let Some(m) = &*models { html! {
                            <select class="picker" ref={picked} title="model for this session">
                                { for m.models.iter().map(|k| html! {
                                    <option value={k.clone()} selected={*k == m.default_model}>{ k }</option>
                                }) }
                            </select>
                        } } else { html!{} } }
                        <span class="spacer"></span>
                        <button class="send" onclick={on_send}>{ "send" }</button>
                    </div>
                </div>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Conversation (one URL per session)
// ---------------------------------------------------------------------------

#[derive(Properties, PartialEq)]
struct ConversationProps {
    id: String,
}

/// Yew's `Reducible` over the pure [`ConvState::apply`]: the component holds
/// one of these, and every event loop holds only its dispatcher — which is
/// stable across renders and applies each action to the *current* state.
struct Conv(ConvState);

impl Reducible for Conv {
    type Action = ConvAction;
    fn reduce(self: Rc<Self>, action: ConvAction) -> Rc<Self> {
        let mut next = self.0.clone();
        next.apply(action);
        Rc::new(Conv(next))
    }
}

#[function_component(Conversation)]
fn conversation(props: &ConversationProps) -> Html {
    let who = use_context::<Session>();
    let me = who.as_ref().map(|s| s.identity.clone());
    // Every piece of live state, one reducer. See `state.rs` for why nothing
    // here may be a `use_state` handle captured by a long-lived task.
    let conv = use_reducer({
        let me = me.clone();
        move || Conv(ConvState::new(me))
    });
    let verbose = use_state(|| false);
    let input = use_node_ref();
    let pane = use_node_ref();
    let at_bottom: Rc<RefCell<bool>> = use_mut_ref(|| true);
    let navigator = use_navigator().unwrap();
    // The work panel. State the poll task writes lives in refs (see
    // `PanelBuf`); the mirrored `use_state` copies exist only so render-time
    // reads and the toggle button stay ordinary Yew.
    let rail_open = use_state(|| false);
    let rail_open_ref: Rc<RefCell<bool>> = use_mut_ref(|| false);
    // The active tab. A ref rather than state: the poll task both reads it
    // (what to fetch) and writes it (auto-select when tabs come and go), and
    // every write bumps `panel_rev` to render.
    let active_tab: Rc<RefCell<Option<Tab>>> = use_mut_ref(|| None);
    let panel: Rc<RefCell<PanelBuf>> = use_mut_ref(PanelBuf::default);
    let panel_rev = use_state(|| 0u64);
    // One render trigger shared by every panel writer (the poll loop, the
    // typing queues, resize echoes): a monotonic counter, so two writers can
    // never set the same value and lose a render.
    let bump: Rc<dyn Fn()> = {
        let counter: Rc<RefCell<u64>> = use_mut_ref(|| 0u64);
        let setter = panel_rev.setter();
        Rc::new(move || {
            let mut c = counter.borrow_mut();
            *c += 1;
            setter.set(*c);
        })
    };
    // The active pty tab's WebSocket, when one is up: the shell key it
    // serves and the sender its frames go through. Keystrokes and resizes
    // prefer it; everything falls back to REST when it is absent.
    type WsSender = futures::channel::mpsc::UnboundedSender<api::ShellWsInput>;
    let ws_link: Rc<RefCell<Option<(String, WsSender)>>> = use_mut_ref(|| None);
    // Keystrokes in flight, per shell (see [`TypeQueue`]).
    let typing: Rc<RefCell<std::collections::HashMap<String, TypeQueue>>> =
        use_mut_ref(std::collections::HashMap::new);
    // The last size we asked each pty for, so the per-render measurements
    // only POST on change.
    let sized: Rc<RefCell<std::collections::HashMap<String, (u16, u16)>>> =
        use_mut_ref(std::collections::HashMap::new);
    // The catalog, for the topbar model picker. One fetch; the catalog does
    // not change while the page is open.
    let models = use_state(Vec::<String>::new);
    {
        let models = models.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                if let Ok(m) = auth::get::<api::Models>("/api/models").await {
                    models.set(m.models);
                }
            });
        });
    }

    // SSE: live deltas; turn boundaries refresh the transcript. The loop owns
    // reconnection — a dropped stream (server restart, proxy hiccup, laptop
    // lid) reloads the transcript and resubscribes, so one machine's blip no
    // longer leaves it silently diverged from every other viewer until reload.
    {
        let dispatch = conv.dispatcher();
        let navigator = navigator.clone();
        let who = who.clone();
        let id = props.id.clone();
        use_effect_with(id, move |id| {
            let id = id.clone();
            let alive = Rc::new(std::cell::Cell::new(true));
            let alive2 = alive.clone();
            spawn_local(async move {
                let mut first_attempt = true;
                'reconnect: while alive2.get() {
                    if !first_attempt {
                        gloo_timers::future::TimeoutFuture::new(1_000).await;
                        if !alive2.get() {
                            break;
                        }
                    }
                    // The transcript on every (re)connect, so history shows
                    // whether or not the feed comes up, and a window missed
                    // while disconnected heals.
                    match fetch_detail(&id).await {
                        Ok(d) => {
                            dispatch.dispatch(ConvAction::Configured {
                                model: d.summary.model,
                                effort: d.summary.effort,
                                context_tokens: d.summary.context_tokens,
                                context_window: d.summary.context_window,
                            });
                            dispatch.dispatch(ConvAction::Loaded {
                                entries: d.entries,
                                busy: d.summary.busy,
                            })
                        }
                        Err(Failure::Unauthorized) => {
                            sign_out_now(&who);
                            return;
                        }
                        Err(e) => {
                            if first_attempt {
                                dispatch.dispatch(ConvAction::Failed(e.to_string()));
                            }
                            first_attempt = false;
                            continue;
                        }
                    }
                    first_attempt = false;

                    let mut es = match EventSource::new(&auth::sse_url(&id)) {
                        Ok(es) => es,
                        Err(_) => continue,
                    };
                    let Ok(mut stream) = es.subscribe("message") else {
                        continue;
                    };
                    while alive2.get() {
                        let Some(Ok((_, msg))) = stream.next().await else {
                            // Closed or errored: fall out to reconnect.
                            drop(es);
                            continue 'reconnect;
                        };
                        let Some(data) = msg.data().as_string() else {
                            continue;
                        };
                        let Ok(ev) = serde_json::from_str::<api::StreamEvent>(&data) else {
                            continue;
                        };
                        match ev {
                            // Somebody posted. Place it in the transcript on
                            // its own terms, right now: it is a message from
                            // a person, not a continuation of whatever the
                            // agent is saying, and it must not wait for the
                            // turn in flight to end.
                            api::StreamEvent::Message { entry, wakes_agent } => {
                                dispatch.dispatch(ConvAction::Message { entry, wakes_agent });
                            }
                            api::StreamEvent::TurnStarted => {
                                dispatch.dispatch(ConvAction::TurnStarted);
                                // Pick up entries that bypassed the feed (a
                                // CLI operator queues turns directly).
                                if let Ok(p) = fetch_poll(&id).await {
                                    dispatch.dispatch(ConvAction::Polled { poll: p });
                                }
                            }
                            api::StreamEvent::TextDelta { text } => {
                                dispatch.dispatch(ConvAction::Text(text))
                            }
                            api::StreamEvent::ThinkingDelta { text } => {
                                dispatch.dispatch(ConvAction::Thinking(text))
                            }
                            // A card from the moment the call starts, in its
                            // running state — the same widget, same key, the
                            // saved entry renders.
                            api::StreamEvent::ToolStarted { id, name, input } => {
                                dispatch.dispatch(ConvAction::ToolStarted { id, name, input })
                            }
                            api::StreamEvent::ToolFinished { result } => {
                                dispatch.dispatch(ConvAction::ToolFinished { result })
                            }
                            api::StreamEvent::TurnFailed { message } => {
                                dispatch.dispatch(ConvAction::TurnFailed(message))
                            }
                            api::StreamEvent::Compacted { successor, .. } => {
                                navigator.push(&Route::Session { id: successor });
                                return;
                            }
                            api::StreamEvent::TurnFinished => {
                                // The wire contract: the turn is persisted
                                // before this event, so one fetch swaps the
                                // stream buffer for its saved form in a
                                // single step. Retry briefly — failing here
                                // would strand the buffer.
                                let mut fetched = None;
                                for _ in 0..5 {
                                    match fetch_poll(&id).await {
                                        Ok(p) => {
                                            fetched = Some(p);
                                            break;
                                        }
                                        Err(_) => {
                                            gloo_timers::future::TimeoutFuture::new(1_000).await
                                        }
                                    }
                                }
                                match fetched {
                                    Some(poll) => dispatch.dispatch(ConvAction::TurnEnded { poll }),
                                    None => dispatch.dispatch(ConvAction::Failed(
                                        "transcript refresh failed".into(),
                                    )),
                                }
                            }
                        }
                    }
                    drop(es);
                }
            });
            move || alive.set(false)
        });
    }

    // The active pty tab's socket: opened when a pty shell becomes the
    // active tab, closed when it stops being one. Screen frames land in the
    // panel buffer; input/resize frames go up the same pipe, ordered by the
    // socket itself. If the socket dies mid-tab the link clears and the REST
    // paths take over until the tab is switched away and back.
    {
        let active_pty: Option<(String, String)> = {
            let p = panel.borrow();
            match active_tab.borrow().as_ref() {
                Some(Tab::Shell { host, id }) => p
                    .shells
                    .iter()
                    .find(|s| s.host == *host && s.id == *id && s.pty)
                    .map(|s| (s.host.clone(), s.id.clone())),
                _ => None,
            }
        };
        let ws_link = ws_link.clone();
        let panel = panel.clone();
        let bump = bump.clone();
        let dispatch = conv.dispatcher();
        let sid = props.id.clone();
        use_effect_with(active_pty, move |active| {
            *ws_link.borrow_mut() = None;
            let alive = Rc::new(std::cell::Cell::new(true));
            if let Some((host, shell)) = active.clone() {
                let alive = alive.clone();
                spawn_local(async move {
                    use futures::{SinkExt, StreamExt};
                    use gloo_net::websocket::{Message as WsMessage, futures::WebSocket};
                    let key = term_key(&host, &shell);
                    let Ok(ws) = WebSocket::open(&auth::shell_ws_url(&sid, &host, &shell)) else {
                        return;
                    };
                    let (mut sink, mut read) = ws.split();
                    let (tx, mut rx) = futures::channel::mpsc::unbounded::<api::ShellWsInput>();
                    *ws_link.borrow_mut() = Some((key.clone(), tx));
                    spawn_local(async move {
                        while let Some(frame) = rx.next().await {
                            let Ok(json) = serde_json::to_string(&frame) else {
                                continue;
                            };
                            if sink.send(WsMessage::Text(json)).await.is_err() {
                                break;
                            }
                        }
                        let _ = sink.close().await;
                    });
                    while alive.get() {
                        let Some(Ok(WsMessage::Text(text))) = read.next().await else {
                            break;
                        };
                        match serde_json::from_str::<api::ShellWsOutput>(&text) {
                            Ok(api::ShellWsOutput::Screen { screen }) => {
                                let mut p = panel.borrow_mut();
                                if p.screens.get(&key) != Some(&screen) {
                                    p.screens.insert(key.clone(), screen);
                                    drop(p);
                                    bump();
                                }
                            }
                            Ok(api::ShellWsOutput::Error { message }) => {
                                dispatch.dispatch(ConvAction::Failed(message));
                            }
                            Err(_) => {}
                        }
                    }
                    // Socket gone: back to REST until the tab is reopened.
                    let mut link = ws_link.borrow_mut();
                    if link.as_ref().is_some_and(|(k, _)| *k == key) {
                        *link = None;
                    }
                });
            }
            move || alive.set(false)
        });
    }

    // The work panel's poll: the shell and subagent lists while the panel is
    // open, plus the active tab's content — a terminal's screen or
    // scrollback tail, or a chat's new entries. Polling, not SSE — the reads
    // are offset-addressed and idempotent, so a fixed cadence is simple and
    // self-healing, and a closed panel costs nothing. Content every 500ms
    // (a terminal wants the present), the lists every other tick.
    {
        let panel = panel.clone();
        let bump = bump.clone();
        let rail_open_ref = rail_open_ref.clone();
        let active_tab = active_tab.clone();
        let ws_link = ws_link.clone();
        let id = props.id.clone();
        use_effect_with(id, move |id| {
            let id = id.clone();
            let alive = Rc::new(std::cell::Cell::new(true));
            let alive2 = alive.clone();
            spawn_local(async move {
                let mut tick = 0u64;
                while alive2.get() {
                    gloo_timers::future::TimeoutFuture::new(500).await;
                    tick += 1;
                    if !alive2.get() {
                        break;
                    }
                    if !*rail_open_ref.borrow() {
                        continue;
                    }
                    let mut changed = false;
                    if tick.is_multiple_of(2) {
                        if let Ok(s) =
                            auth::get::<api::Shells>(&format!("/api/sessions/{id}/shells")).await
                        {
                            let mut p = panel.borrow_mut();
                            if p.shells != s.shells {
                                p.shells = s.shells;
                                changed = true;
                            }
                        }
                        if let Ok(s) =
                            auth::get::<api::Subagents>(&format!("/api/sessions/{id}/subagents"))
                                .await
                        {
                            let mut p = panel.borrow_mut();
                            if p.subs != s.subagents {
                                p.subs = s.subagents;
                                changed = true;
                            }
                        }
                    }
                    // Tabs come and go with the lists; keep the active one
                    // honest — dead tabs drop, and the first live thing is
                    // selected when nothing is.
                    {
                        let p = panel.borrow();
                        let mut at = active_tab.borrow_mut();
                        let valid = |t: &Tab| match t {
                            Tab::Shell { host, id } => {
                                p.shells.iter().any(|s| s.host == *host && s.id == *id)
                            }
                            Tab::Sub { id } => p.subs.iter().any(|s| s.id == *id),
                        };
                        let want = if at.as_ref().is_some_and(valid) {
                            at.clone()
                        } else {
                            p.shells
                                .first()
                                .map(|s| Tab::Shell {
                                    host: s.host.clone(),
                                    id: s.id.clone(),
                                })
                                .or_else(|| p.subs.first().map(|s| Tab::Sub { id: s.id.clone() }))
                        };
                        if *at != want {
                            *at = want;
                            changed = true;
                        }
                    }
                    let tab = active_tab.borrow().clone();
                    match tab {
                        Some(Tab::Shell { host, id: shell }) => {
                            let key = term_key(&host, &shell);
                            // A pty session renders as its current screen
                            // (the snapshot is the view); a piped one
                            // accumulates its scrollback tail.
                            let pty = panel
                                .borrow()
                                .shells
                                .iter()
                                .any(|s| s.host == host && s.id == shell && s.pty);
                            let socket_live =
                                ws_link.borrow().as_ref().is_some_and(|(k, _)| *k == key);
                            if pty {
                                // The socket pushes screens; polling one on
                                // top would just re-render the same bytes.
                                if !socket_live
                                    && let Ok(screen) = auth::get::<api::ShellScreen>(&format!(
                                        "/api/sessions/{id}/shells/{host}/{shell}/screen"
                                    ))
                                    .await
                                {
                                    let mut p = panel.borrow_mut();
                                    if p.screens.get(&key) != Some(&screen) {
                                        p.screens.insert(key, screen);
                                        changed = true;
                                    }
                                }
                            } else {
                                let from =
                                    panel.borrow().terms.get(&key).map(|t| t.from).unwrap_or(0);
                                if let Ok(chunk) = auth::get::<api::ShellTailChunk>(&format!(
                                    "/api/sessions/{id}/shells/{host}/{shell}?from={from}"
                                ))
                                .await
                                    && chunk.end != from
                                {
                                    panel
                                        .borrow_mut()
                                        .terms
                                        .entry(key)
                                        .or_default()
                                        .absorb(&chunk);
                                    changed = true;
                                }
                            }
                        }
                        Some(Tab::Sub { id: child }) => {
                            let since = panel
                                .borrow()
                                .chats
                                .get(&child)
                                .map(|c| c.since)
                                .unwrap_or(0);
                            if let Ok(p) = auth::get::<api::Poll>(&format!(
                                "/api/sessions/{child}/poll?since={since}"
                            ))
                            .await
                                && (p.total != since || !p.entries.is_empty())
                            {
                                let mut buf = panel.borrow_mut();
                                let chat = buf.chats.entry(child.clone()).or_default();
                                chat.entries.extend(p.entries);
                                chat.since = p.total;
                                changed = true;
                            }
                        }
                        None => {}
                    }
                    if changed {
                        bump();
                    }
                }
            });
            move || alive.set(false)
        });
    }

    // Slow reconciliation poll: covers anything the feed cannot — a lagged
    // broadcast, entries written by another process. The reducer stands it
    // down while a turn streams (see `ConvAction::Polled`).
    {
        let dispatch = conv.dispatcher();
        let id = props.id.clone();
        use_effect_with(id, move |id| {
            let id = id.clone();
            let alive = Rc::new(std::cell::Cell::new(true));
            let alive2 = alive.clone();
            spawn_local(async move {
                while alive2.get() {
                    gloo_timers::future::TimeoutFuture::new(5_000).await;
                    if !alive2.get() {
                        break;
                    }
                    if let Ok(p) = fetch_poll(&id).await {
                        dispatch.dispatch(ConvAction::Polled { poll: p });
                    }
                }
            });
            move || alive.set(false)
        });
    }

    // Terminal behaviour: ride the bottom as output lands, but leave a reader
    // who has scrolled up where they are.
    {
        let pane = pane.clone();
        let at_bottom = at_bottom.clone();
        let streamed: usize = conv.0.streaming.iter().map(StreamItem::size).sum();
        use_effect_with(
            (conv.0.entries.len(), conv.0.streaming.len(), streamed),
            move |_| {
                if *at_bottom.borrow()
                    && let Some(el) = pane.cast::<web_sys::Element>()
                {
                    el.set_scroll_top(el.scroll_height());
                }
            },
        );
    }

    // Track whether the pane is pinned to the bottom (within a line or two).
    let on_scroll = {
        let pane = pane.clone();
        let at_bottom = at_bottom.clone();
        Callback::from(move |_: Event| {
            if let Some(el) = pane.cast::<web_sys::Element>() {
                let slack = el.scroll_height() - el.client_height() - el.scroll_top();
                *at_bottom.borrow_mut() = slack < 40;
            }
        })
    };

    let send_now: Callback<()> = {
        let id = props.id.clone();
        let input = input.clone();
        let dispatch = conv.dispatcher();
        Callback::from(move |_| {
            let Some(ta) = input.cast::<HtmlTextAreaElement>() else {
                return;
            };
            let text = ta.value();
            if text.trim().is_empty() {
                return;
            }
            ta.set_value("");
            let id = id.clone();
            let dispatch = dispatch.clone();
            spawn_local(async move {
                match auth::post_json::<api::Poll, _>(
                    &format!("/api/sessions/{id}/messages"),
                    &api::PostMessage { text },
                )
                .await
                {
                    // The server decides whether this message wakes the agent
                    // — or lands while it is already answering someone — and
                    // reports that as `busy`. Believing it keeps the composer
                    // honest when a message was meant for the room.
                    Ok(p) => dispatch.dispatch(ConvAction::Sent { busy: p.busy }),
                    Err(e) => dispatch.dispatch(ConvAction::Failed(format!("send failed: {e}"))),
                }
            });
        })
    };

    let on_send = to_click(&send_now);
    let on_keydown = to_enter(&send_now);

    let toggle_verbose = {
        let verbose = verbose.clone();
        Callback::from(move |_: MouseEvent| verbose.set(!*verbose))
    };

    // Reconfigure the session from a topbar picker. The server validates,
    // persists, and tells the (possibly running) agent task; the `Configured`
    // dispatch adopts whatever it answered — the pickers show the server's
    // truth, not the click.
    let reconfigure = {
        let id = props.id.clone();
        let dispatch = conv.dispatcher();
        move |req_of: fn(String) -> api::UpdateSession| {
            let id = id.clone();
            let dispatch = dispatch.clone();
            Callback::from(move |e: Event| {
                let Some(sel) = e.target_dyn_into::<HtmlSelectElement>() else {
                    return;
                };
                let req = req_of(sel.value());
                let id = id.clone();
                let dispatch = dispatch.clone();
                spawn_local(async move {
                    match auth::patch_json::<api::SessionSummary, _>(
                        &format!("/api/sessions/{id}"),
                        &req,
                    )
                    .await
                    {
                        Ok(s) => dispatch.dispatch(ConvAction::Configured {
                            model: s.model,
                            effort: s.effort,
                            context_tokens: s.context_tokens,
                            context_window: s.context_window,
                        }),
                        Err(e) => {
                            dispatch.dispatch(ConvAction::Failed(format!("reconfigure: {e}")))
                        }
                    }
                });
            })
        }
    };
    let on_pick_model = reconfigure(|model| api::UpdateSession {
        model: Some(model),
        ..Default::default()
    });
    // The "default" option's value is the empty string — the wire's spelling
    // for clearing the override.
    let on_pick_effort = reconfigure(|effort| api::UpdateSession {
        effort: Some(effort),
        ..Default::default()
    });

    let on_cancel = {
        let id = props.id.clone();
        Callback::from(move |_: MouseEvent| {
            let id = id.clone();
            spawn_local(async move {
                let _ = auth::post::<api::Poll>(&format!("/api/sessions/{id}/cancel")).await;
            });
        })
    };

    let toggle_rail = {
        let rail_open = rail_open.clone();
        let rail_open_ref = rail_open_ref.clone();
        Callback::from(move |_: MouseEvent| {
            let now = !*rail_open;
            *rail_open_ref.borrow_mut() = now;
            rail_open.set(now);
        })
    };

    // Jump from a rail row to its kickoff card in the transcript.
    let jump_to_card = |call_id: String| {
        Callback::from(move |_: MouseEvent| {
            if let Some(doc) = web_sys::window().and_then(|w| w.document())
                && let Some(el) = doc.get_element_by_id(&format!("tool-{call_id}"))
            {
                el.scroll_into_view();
            }
        })
    };

    let select_tab = |tab: Tab| {
        let active_tab = active_tab.clone();
        let panel_rev = panel_rev.clone();
        Callback::from(move |_: MouseEvent| {
            *active_tab.borrow_mut() = Some(tab.clone());
            panel_rev.set(*panel_rev + 1);
        })
    };

    // Point a shell's keyboard. The optimistic lock flip makes the head
    // answer the click; the 1s poll is the truth that heals a failed POST.
    let shell_lock = |host: String, shell: String| {
        let id = props.id.clone();
        let panel = panel.clone();
        let panel_rev = panel_rev.clone();
        let dispatch = conv.dispatcher();
        Callback::from(move |take: bool| {
            let lock = if take {
                api::ShellLockMode::User
            } else {
                api::ShellLockMode::Assistant
            };
            {
                let mut p = panel.borrow_mut();
                if let Some(s) = p
                    .shells
                    .iter_mut()
                    .find(|s| s.host == host && s.id == shell)
                {
                    s.lock = lock;
                }
            }
            panel_rev.set(*panel_rev + 1);
            let id = id.clone();
            let host = host.clone();
            let shell = shell.clone();
            let dispatch = dispatch.clone();
            spawn_local(async move {
                if let Err(e) = auth::post_json::<api::Shell, _>(
                    &format!("/api/sessions/{id}/shells/{host}/{shell}/lock"),
                    &api::ShellLockRequest { lock },
                )
                .await
                {
                    dispatch.dispatch(ConvAction::Failed(format!("shell lock: {e}")));
                }
            });
        })
    };

    let shell_send = {
        let id = props.id.clone();
        let dispatch = conv.dispatcher();
        move |host: String, shell: String| {
            let id = id.clone();
            let dispatch = dispatch.clone();
            Callback::from(move |data: String| {
                let id = id.clone();
                let host = host.clone();
                let shell = shell.clone();
                let dispatch = dispatch.clone();
                spawn_local(async move {
                    if let Err(e) = auth::post_json::<api::Shell, _>(
                        &format!("/api/sessions/{id}/shells/{host}/{shell}/input"),
                        &api::ShellInput {
                            data: format!("{data}\n"),
                        },
                    )
                    .await
                    {
                        dispatch.dispatch(ConvAction::Failed(format!("shell input: {e}")));
                    }
                });
            })
        }
    };

    // Raw keystrokes into a pty, in typed order: each shell has one queue
    // and at most one POST in flight — a fast typist appends while the
    // previous chunk sends, and the drain loop carries the rest. Every
    // successful send refetches the screen at once, so the echo latency is
    // one round trip, not one poll tick.
    let shell_keys = {
        let id = props.id.clone();
        let typing = typing.clone();
        let panel = panel.clone();
        let bump = bump.clone();
        let ws_link = ws_link.clone();
        let dispatch = conv.dispatcher();
        move |host: String, shell: String| {
            let id = id.clone();
            let typing = typing.clone();
            let panel = panel.clone();
            let bump = bump.clone();
            let ws_link = ws_link.clone();
            let dispatch = dispatch.clone();
            Callback::from(move |keys: String| {
                let key = term_key(&host, &shell);
                // The socket is the fast path: ordered frames, echo pushed
                // back the moment the pty produces it.
                {
                    let mut link = ws_link.borrow_mut();
                    if let Some((k, tx)) = link.as_ref()
                        && *k == key
                    {
                        if tx
                            .unbounded_send(api::ShellWsInput::Input { data: keys.clone() })
                            .is_ok()
                        {
                            return;
                        }
                        *link = None;
                    }
                }
                {
                    let mut q = typing.borrow_mut();
                    let queue = q.entry(key.clone()).or_default();
                    queue.buf.push_str(&keys);
                    if queue.busy {
                        return;
                    }
                    queue.busy = true;
                }
                let id = id.clone();
                let host = host.clone();
                let shell = shell.clone();
                let typing = typing.clone();
                let panel = panel.clone();
                let bump = bump.clone();
                let dispatch = dispatch.clone();
                spawn_local(async move {
                    loop {
                        let data = {
                            let mut q = typing.borrow_mut();
                            let queue = q.entry(key.clone()).or_default();
                            if queue.buf.is_empty() {
                                queue.busy = false;
                                break;
                            }
                            std::mem::take(&mut queue.buf)
                        };
                        let sent = auth::post_json::<api::Shell, _>(
                            &format!("/api/sessions/{id}/shells/{host}/{shell}/input"),
                            &api::ShellInput { data },
                        )
                        .await;
                        if let Err(e) = sent {
                            typing.borrow_mut().entry(key.clone()).or_default().busy = false;
                            dispatch.dispatch(ConvAction::Failed(format!("shell input: {e}")));
                            break;
                        }
                        if let Ok(screen) = auth::get::<api::ShellScreen>(&format!(
                            "/api/sessions/{id}/shells/{host}/{shell}/screen"
                        ))
                        .await
                        {
                            let mut p = panel.borrow_mut();
                            if p.screens.get(&key) != Some(&screen) {
                                p.screens.insert(key.clone(), screen);
                                drop(p);
                                bump();
                            }
                        }
                    }
                });
            })
        }
    };

    // Fit a pty to the panel. WorkView measures every render; this dedupes
    // against the last size asked for and lets errors pass silently — a
    // resize that lost a race with an agent write retries on the next
    // measurement.
    let shell_fit = {
        let id = props.id.clone();
        let sized = sized.clone();
        let panel = panel.clone();
        let bump = bump.clone();
        let ws_link = ws_link.clone();
        move |host: String, shell: String| {
            let id = id.clone();
            let sized = sized.clone();
            let panel = panel.clone();
            let bump = bump.clone();
            let ws_link = ws_link.clone();
            Callback::from(move |(cols, rows): (u16, u16)| {
                let key = term_key(&host, &shell);
                {
                    let mut s = sized.borrow_mut();
                    let current = panel
                        .borrow()
                        .screens
                        .get(&key)
                        .map(|scr| (scr.cols, scr.rows));
                    if s.get(&key) == Some(&(cols, rows)) && current == Some((cols, rows)) {
                        return;
                    }
                    if current == Some((cols, rows)) {
                        s.insert(key.clone(), (cols, rows));
                        return;
                    }
                    s.insert(key.clone(), (cols, rows));
                }
                {
                    let link = ws_link.borrow();
                    if let Some((k, tx)) = link.as_ref()
                        && *k == key
                        && tx
                            .unbounded_send(api::ShellWsInput::Resize { cols, rows })
                            .is_ok()
                    {
                        return;
                    }
                }
                let id = id.clone();
                let host = host.clone();
                let shell = shell.clone();
                let panel = panel.clone();
                let bump = bump.clone();
                spawn_local(async move {
                    if auth::post_json::<api::Shell, _>(
                        &format!("/api/sessions/{id}/shells/{host}/{shell}/resize"),
                        &api::ShellResize { cols, rows },
                    )
                    .await
                    .is_ok()
                        && let Ok(screen) = auth::get::<api::ShellScreen>(&format!(
                            "/api/sessions/{id}/shells/{host}/{shell}/screen"
                        ))
                        .await
                    {
                        panel.borrow_mut().screens.insert(key, screen);
                        bump();
                    }
                });
            })
        }
    };

    // The subagent twins of the two above, against the child's lock and
    // composer endpoints on the *parent* session.
    let sub_lock = |child: String| {
        let id = props.id.clone();
        let panel = panel.clone();
        let panel_rev = panel_rev.clone();
        let dispatch = conv.dispatcher();
        Callback::from(move |take: bool| {
            let lock = if take {
                api::ShellLockMode::User
            } else {
                api::ShellLockMode::Assistant
            };
            {
                let mut p = panel.borrow_mut();
                if let Some(s) = p.subs.iter_mut().find(|s| s.id == child) {
                    s.lock = lock;
                }
            }
            panel_rev.set(*panel_rev + 1);
            let id = id.clone();
            let child = child.clone();
            let dispatch = dispatch.clone();
            spawn_local(async move {
                if let Err(e) = auth::post_json::<api::Subagent, _>(
                    &format!("/api/sessions/{id}/subagents/{child}/lock"),
                    &api::ShellLockRequest { lock },
                )
                .await
                {
                    dispatch.dispatch(ConvAction::Failed(format!("subagent lock: {e}")));
                }
            });
        })
    };

    // Open a terminal for the user (on the local host; remote shells are
    // still opened by the agent, which is what remotes are for). The name
    // prompt is native chrome on purpose — a modal input is not worth a
    // component.
    let open_term = {
        let id = props.id.clone();
        let active_tab = active_tab.clone();
        let panel_rev = panel_rev.clone();
        let panel = panel.clone();
        let dispatch = conv.dispatcher();
        Callback::from(move |_: MouseEvent| {
            let name = web_sys::window()
                .and_then(|w| w.prompt_with_message_and_default("terminal name", "").ok())
                .flatten()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty());
            let id = id.clone();
            let active_tab = active_tab.clone();
            let panel_rev = panel_rev.clone();
            let panel = panel.clone();
            let dispatch = dispatch.clone();
            spawn_local(async move {
                match auth::post_json::<api::Shell, _>(
                    &format!("/api/sessions/{id}/shells/local"),
                    &api::CreateShell {
                        shell: name,
                        command: None,
                        pty: true,
                        cols: None,
                        rows: None,
                    },
                )
                .await
                {
                    Ok(shell) => {
                        let mut p = panel.borrow_mut();
                        let tab = Tab::Shell {
                            host: shell.host.clone(),
                            id: shell.id.clone(),
                        };
                        p.shells.push(shell);
                        drop(p);
                        *active_tab.borrow_mut() = Some(tab);
                        panel_rev.set(*panel_rev + 1);
                    }
                    Err(e) => dispatch.dispatch(ConvAction::Failed(format!("open terminal: {e}"))),
                }
            });
        })
    };

    let rename_term = {
        let id = props.id.clone();
        let panel = panel.clone();
        let panel_rev = panel_rev.clone();
        let dispatch = conv.dispatcher();
        move |host: String, shell: String, current: Option<String>| {
            let id = id.clone();
            let panel = panel.clone();
            let panel_rev = panel_rev.clone();
            let dispatch = dispatch.clone();
            Callback::from(move |_: ()| {
                let Some(title) = web_sys::window().and_then(|w| {
                    w.prompt_with_message_and_default(
                        "terminal name (empty clears)",
                        current.as_deref().unwrap_or(""),
                    )
                    .ok()
                    .flatten()
                }) else {
                    return; // cancelled
                };
                let id = id.clone();
                let host = host.clone();
                let shell = shell.clone();
                let panel = panel.clone();
                let panel_rev = panel_rev.clone();
                let dispatch = dispatch.clone();
                spawn_local(async move {
                    match auth::post_json::<api::Shell, _>(
                        &format!("/api/sessions/{id}/shells/{host}/{shell}/rename"),
                        &api::ShellRename { title },
                    )
                    .await
                    {
                        Ok(fresh) => {
                            let mut p = panel.borrow_mut();
                            if let Some(s) = p
                                .shells
                                .iter_mut()
                                .find(|s| s.host == fresh.host && s.id == fresh.id)
                            {
                                s.title = fresh.title;
                            }
                            drop(p);
                            panel_rev.set(*panel_rev + 1);
                        }
                        Err(e) => dispatch.dispatch(ConvAction::Failed(format!("rename: {e}"))),
                    }
                });
            })
        }
    };

    let sub_send = {
        let id = props.id.clone();
        let dispatch = conv.dispatcher();
        move |child: String| {
            let id = id.clone();
            let dispatch = dispatch.clone();
            Callback::from(move |text: String| {
                if text.trim().is_empty() {
                    return;
                }
                let id = id.clone();
                let child = child.clone();
                let dispatch = dispatch.clone();
                spawn_local(async move {
                    if let Err(e) = auth::post_json::<api::Subagent, _>(
                        &format!("/api/sessions/{id}/subagents/{child}/input"),
                        &api::PostMessage { text },
                    )
                    .await
                    {
                        dispatch.dispatch(ConvAction::Failed(format!("subagent input: {e}")));
                    }
                });
            })
        }
    };

    let st = &conv.0;

    // The running half of the rail comes straight from the stream buffer:
    // a card without a result is a call still in flight.
    let running: Vec<(String, String)> = st
        .streaming
        .iter()
        .filter_map(|item| match item {
            StreamItem::Tool {
                id,
                name,
                result: None,
                ..
            } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();

    // The work panel: a horizontal split beside the chat, VSCode-style — a
    // tab per live shell and live subagent (tabs come and go with the
    // lists), the active one filling the panel as its own terminal or chat.
    let rail = if *rail_open {
        let p = panel.borrow();
        let active = active_tab.borrow().clone();
        let tabs: Vec<Html> = p
            .shells
            .iter()
            .map(|shell| {
                let tab = Tab::Shell {
                    host: shell.host.clone(),
                    id: shell.id.clone(),
                };
                let name = shell.title.clone().unwrap_or_else(|| shell.id.clone());
                let label = if shell.host == "local" {
                    name
                } else {
                    format!("{name}@{}", shell.host)
                };
                (tab, label, shell.running, shell.lock)
            })
            .chain(p.subs.iter().map(|sub| {
                let label = format!("sub:{}", &sub.id[..sub.id.len().min(8)]);
                (Tab::Sub { id: sub.id.clone() }, label, sub.busy, sub.lock)
            }))
            .map(|(tab, label, running, lock)| {
                let is_active = active.as_ref() == Some(&tab);
                let user_locked = lock == api::ShellLockMode::User;
                html! {
                    <div class={ classes!("work-tab",
                                          is_active.then_some("work-tab-active"),
                                          user_locked.then_some("work-tab-user")) }
                         onclick={select_tab(tab)}>
                        <span class={ if running { "tool-disc tool-disc-running" }
                                      else { "tool-disc tool-disc-ok" } }>{ "●" }</span>
                        { label }
                    </div>
                }
            })
            .collect();

        let view = match &active {
            Some(Tab::Shell { host, id }) => {
                p.shells.iter().find(|s| s.host == *host && s.id == *id).map(|shell| {
                    let key = term_key(host, id);
                    let screen = shell.pty.then(|| p.screens.get(&key)).flatten();
                    let body = if shell.pty {
                        screen.map(render_screen)
                            .unwrap_or_else(|| html! { <span class="dim">{ "…" }</span> })
                    } else {
                        html! { { p.terms.get(&key).map(|t| t.text.clone()).unwrap_or_default() } }
                    };
                    html! {
                        <WorkView running={shell.running}
                                  title={shell.title.clone().unwrap_or_else(|| shell.id.clone())}
                                  badge={(shell.host != "local").then(|| format!("@{}", shell.host))}
                                  detail={shell.cmdline.clone()}
                                  user_locked={shell.lock == api::ShellLockMode::User}
                                  on_rename={rename_term(shell.host.clone(), shell.id.clone(), shell.title.clone())}
                                  pty={shell.pty}
                                  app_cursor={screen.is_some_and(|s| s.application_cursor)}
                                  on_lock={shell_lock(shell.host.clone(), shell.id.clone())}
                                  on_send={shell_send(shell.host.clone(), shell.id.clone())}
                                  on_keys={shell_keys(shell.host.clone(), shell.id.clone())}
                                  on_resize={shell_fit(shell.host.clone(), shell.id.clone())}
                                  placeholder="type into the shell (Enter sends · Esc hands the keyboard back)">
                            { body }
                        </WorkView>
                    }
                })
            }
            Some(Tab::Sub { id }) => {
                p.subs.iter().find(|s| s.id == *id).map(|sub| {
                    let body = match p.chats.get(&sub.id) {
                        Some(c) => render_transcript(&c.entries, &[], &[], *verbose,
                                                     who.as_ref().map(|s| &s.identity)),
                        None => html! { <span class="dim">{ "…" }</span> },
                    };
                    html! {
                        <WorkView running={sub.busy}
                                  title={format!("subagent {}", &sub.id[..sub.id.len().min(8)])}
                                  badge={Some(sub.model.clone())}
                                  user_locked={sub.lock == api::ShellLockMode::User}
                                  chat=true
                                  on_lock={sub_lock(sub.id.clone())}
                                  on_send={sub_send(sub.id.clone())}
                                  placeholder="message the subagent (Enter sends · Esc hands it back)">
                            { body }
                            <div class="chat-open-link">
                                <Link<Route> to={Route::Session { id: sub.id.clone() }}>
                                    { "open as full session ↗" }
                                </Link<Route>>
                            </div>
                        </WorkView>
                    }
                })
            }
            None => None,
        };

        html! {
            <aside class="work">
                { if !running.is_empty() { html! {
                    <div class="work-strip">
                        { for running.iter().map(|(call_id, name)| html! {
                            <div class="rail-tool" onclick={jump_to_card(call_id.clone())}>
                                <span class="tool-disc tool-disc-running">{ "●" }</span>
                                <span class="tool-name">{ name }</span>
                            </div>
                        }) }
                    </div>
                } } else { html!{} } }
                <div class="work-tabs">
                    { for tabs.into_iter() }
                    <div class="work-tab work-tab-new" title="open a terminal (yours until you hand it over)"
                         onclick={open_term.clone()}>{ "+" }</div>
                </div>
                { view.unwrap_or_else(|| html! {
                    <div class="empty work-empty">{ "no live shells or subagents" }</div>
                }) }
            </aside>
        }
    } else {
        html! {}
    };

    html! {
        <div class="app">
            <div class="topbar">
                <div class="column">
                    <Link<Route> to={Route::Browser}>{ "← sessions" }</Link<Route>>
                    <span class="dim">{ format!(" {} ", props.id) }</span>
                    { if st.busy { html!{ <span class="dim">{ "· working…" }</span> } } else { html!{} } }
                    { if let Some(used) = st.context_tokens { html! {
                        <span class="dim" title="context tokens in use / context window">
                            { match st.context_window {
                                Some(w) if w > 0 => format!("· {}/{} ctx", fmt_tokens(used), fmt_tokens(w)),
                                _ => format!("· {} ctx", fmt_tokens(used)),
                            } }
                        </span>
                    } } else { html!{} } }
                    <button class="linkish" onclick={toggle_verbose}>
                        { if *verbose { "· concise" } else { "· verbose" } }
                    </button>
                    <button class="linkish" onclick={toggle_rail}>
                        { if *rail_open { "· hide work" } else { "· work" } }
                    </button>
                    { " " }
                    <select class="picker" onchange={on_pick_model} title="model (applies from the next turn)">
                        { for models.iter().map(|m| html! {
                            <option value={m.clone()} selected={*m == st.model}>{ m }</option>
                        }) }
                        // A session may run a key since removed from the
                        // catalog; show it rather than silently selecting
                        // something else.
                        { if !st.model.is_empty() && !models.contains(&st.model) { html! {
                            <option value={st.model.clone()} selected=true>{ &st.model }</option>
                        } } else { html!{} } }
                    </select>
                    { " " }
                    <select class="picker" onchange={on_pick_effort} title="reasoning effort (applies from the next turn)">
                        <option value="" selected={st.effort.is_none()}>{ "effort: default" }</option>
                        { for ["low", "medium", "high", "max"].iter().map(|e| html! {
                            <option value={*e} selected={st.effort.as_deref() == Some(*e)}>
                                { format!("effort: {e}") }
                            </option>
                        }) }
                    </select>
                    { whoami_line(&who) }
                </div>
            </div>
            <div class="pane-row">
            <div class="pane" ref={pane} onscroll={on_scroll}>
                <div class="column">
                { render_transcript(&st.entries, &st.streaming, &st.arrivals, *verbose,
                                    who.as_ref().map(|s| &s.identity)) }
                { if let Some(e) = &st.error { html! {
                    <div class="err"><hr class="rule" /><b>{ "ERROR" }</b><pre>{ e }</pre></div>
                } } else { html!{} } }
                // The composer sits inline under the last message, inside
                // the chat's own scroll — so the work panel beside it owns
                // the full window height.
                <div class="composer composer-inline">
                    <textarea ref={input} rows="2" onkeydown={on_keydown}
                              placeholder={if st.shared {
                                  "message the room (@myco to ask the agent)"
                              } else {
                                  "message (Enter to send, Shift+Enter for a newline)"
                              }} />
                    <div class="actions">
                        { if st.shared { html! {
                            <span class="dim hint">
                                { "shared session · the agent replies when you say " }
                                <span class="mention">{ "@myco" }</span>
                            </span>
                        } } else { html!{} } }
                        <span class="spacer"></span>
                        <button class="send" onclick={on_send}>{ "send" }</button>
                        { if st.busy {
                            html! { <button onclick={on_cancel}>{ "cancel" }</button> }
                        } else { html!{} } }
                    </div>
                </div>
                </div>
            </div>
            { rail }
            </div>
        </div>
    }
}
