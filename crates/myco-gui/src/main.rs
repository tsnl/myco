//! Minimal Yew client for the myco API server: a session browser at `/` and
//! one conversation per URL at `/session/<id>`. Live output arrives over SSE;
//! a slow poll reconciles the transcript as a fallback.
//!
//! The visual identity is the terminal's: monospace, dark, USER/ASSISTANT
//! section rules — the web page renders (approximately) what the CLI prints,
//! with no further chrome.

mod auth;
mod highlight;
mod notify;
mod state;

use std::cell::RefCell;
use std::rc::Rc;

use state::{ConvAction, ConvState, StreamItem};

use futures::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use myco_api as api;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
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

    html! {
        <div class="app">
            <div class="pane">
                <div class="column login">
                    <h1>{ "myco" }</h1>
                    <div class="login-row">
                        <input ref={username} type="text" autocomplete="username"
                               onkeydown={on_keydown.clone()} placeholder="username" />
                    </div>
                    <div class="login-row">
                        <input ref={password} type="password" autocomplete="current-password"
                               onkeydown={on_keydown} placeholder="password" />
                    </div>
                    <div class="login-row">
                        <button onclick={on_click} disabled={*busy}>
                            { if *busy { "signing in…" } else { "sign in" } }
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

/// `signed in as <name>`, with the sign-out affordance next to it.
fn whoami_line(who: &Option<Session>) -> Html {
    let Some(s) = who else { return html! {} };
    let sign_out = {
        let cb = s.sign_out.clone();
        Callback::from(move |_: MouseEvent| cb.emit(()))
    };
    html! {
        <span class="dim">
            { format!(" · {} ", s.identity.name) }
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

/// Markdown → HTML, with raw HTML in the source neutralized (a model must
/// not be able to inject markup into the page) and fenced code blocks
/// syntax-highlighted.
///
/// The highlighter is the only thing allowed to emit markup here, and it
/// escapes the code it is given — so the un-trusted text still cannot become
/// tags, it just gets colored on the way through.
fn markdown(src: &str) -> Html {
    use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

    let mut events = Vec::new();
    // `Some` while inside a fence: the language and the code collected so far.
    let mut fence: Option<(String, String)> = None;
    for ev in Parser::new(src) {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match &kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                fence = Some((lang, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, code)) = fence.take() {
                    let body = highlight::highlight_to_html(&code, &lang);
                    events.push(Event::Html(
                        format!("<pre class=\"code\"><code>{body}</code></pre>").into(),
                    ));
                }
            }
            Event::Text(t) if fence.is_some() => {
                if let Some((_, code)) = fence.as_mut() {
                    code.push_str(&t);
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => events.push(Event::Text(t)),
            other => events.push(other),
        }
    }
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events.into_iter());
    Html::from_html_unchecked(AttrValue::from(format!("<div class=\"md\">{out}</div>")))
}

/// Pretty-printed, highlighted JSON in a `<pre>`.
/// A tool call's arguments, highlighted as YAML. Falls back to escaped plain
/// text when the syntax set has no YAML, which is the highlighter's own
/// contract — the arguments are readable either way.
fn yaml_block(text: &str) -> Html {
    let body = highlight::highlight_to_html(text, "yaml");
    Html::from_html_unchecked(AttrValue::from(format!(
        "<pre class=\"code yaml\">{body}</pre>"
    )))
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

/// Lines of a collapsed tool result before it is cut off. Enough to see what
/// happened, short enough that a 5,000-line build log does not bury the
/// conversation.
const RESULT_PREVIEW_LINES: usize = 8;

#[derive(Properties, PartialEq)]
struct ToolCardProps {
    tool: api::ToolUse,
    /// The matching result, once the tool has finished.
    result: Option<api::ToolResult>,
    /// The transcript-wide verbose setting. Flipping it re-seeds every card,
    /// but a card the reader has opened by hand keeps its own state until
    /// then.
    verbose: bool,
}

/// How a call is going, as one glance.
#[derive(Clone, Copy, PartialEq)]
enum ToolStatus {
    Running,
    Ok,
    Failed,
}

impl ToolStatus {
    fn of(result: Option<&api::ToolResult>) -> Self {
        match result {
            None => ToolStatus::Running,
            Some(r) if r.is_error => ToolStatus::Failed,
            Some(_) => ToolStatus::Ok,
        }
    }

    fn disc_class(self) -> &'static str {
        match self {
            ToolStatus::Running => "tool-disc tool-disc-running",
            ToolStatus::Ok => "tool-disc tool-disc-ok",
            ToolStatus::Failed => "tool-disc tool-disc-failed",
        }
    }

    /// Colour alone is not a label. The disc carries a title so the state is
    /// readable to a screen reader and on hover.
    fn title(self) -> &'static str {
        match self {
            ToolStatus::Running => "running",
            ToolStatus::Ok => "completed successfully",
            ToolStatus::Failed => "failed",
        }
    }
}

/// One tool call as its own bordered block: a status disc, the name, the
/// arguments, and the result folded in underneath.
///
/// The call and its output are collapsed on different terms, because they are
/// read for different reasons. The arguments are *what was asked for* — short,
/// and the thing you scan a transcript to find — so they are always shown in
/// full. The output is *what came back*, which can be a 5,000-line build log,
/// so it is capped until asked for. Only the result has a toggle.
#[function_component(ToolCard)]
fn tool_card(props: &ToolCardProps) -> Html {
    let expanded = use_state(|| props.verbose);
    {
        let expanded = expanded.clone();
        use_effect_with(props.verbose, move |v| expanded.set(*v));
    }
    let toggle = {
        let expanded = expanded.clone();
        Callback::from(move |_: MouseEvent| expanded.set(!*expanded))
    };
    let open = *expanded;

    // Always the whole call, never summarized: a truncated command is one you
    // have to expand the card to trust. How it reads is the tool's own
    // business — `bash` lays its command out as shell, everything else falls
    // back to the structural rendering.
    let args = api::tool_display::tool_input_yaml(
        &props.tool.name,
        &props.tool.input,
        api::tool_display::TOOL_DISPLAY_WIDTH,
    );
    let status = ToolStatus::of(props.result.as_ref());
    let is_error = status == ToolStatus::Failed;

    let result_body = props.result.as_ref().map(|r| {
        // Images render as images (a `view_image` preview belongs on screen,
        // not as an `[image]` note), so the text preview reads Text parts only.
        let text = text_parts(&r.content);
        let images: Vec<Html> = r
            .content
            .iter()
            .filter_map(|c| match c {
                api::Content::Image { source } => Some(render_image(source)),
                _ => None,
            })
            .collect();
        let total = text.lines().count();
        let shown = if open || total <= RESULT_PREVIEW_LINES {
            text.clone()
        } else {
            text.lines()
                .take(RESULT_PREVIEW_LINES)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let hidden = total.saturating_sub(RESULT_PREVIEW_LINES);
        html! {
            <div class="tool-result">
                { if !shown.is_empty() { html! {
                    <pre class={ if r.is_error { "err" } else { "dim" } }>{ shown }</pre>
                } } else { html!{} } }
                { for images.into_iter() }
                { if !open && hidden > 0 { html! {
                    <button class="linkish" onclick={toggle.clone()}>
                        { format!("+{hidden} more lines") }
                    </button>
                } } else { html!{} } }
            </div>
        }
    });

    // Nothing to fold away until output exists, and a long result is the only
    // thing the toggle acts on.
    let foldable = props
        .result
        .as_ref()
        .is_some_and(|r| text_parts(&r.content).lines().count() > RESULT_PREVIEW_LINES);

    // The DOM id is the jump target for the work rail's running-tool rows.
    html! {
        <div id={format!("tool-{}", props.tool.id)}
             class={ classes!("tool-card", is_error.then_some("tool-card-error")) }>
            <div class="tool-head" onclick={toggle.clone()}>
                <span class={status.disc_class()} title={status.title()}>{ "●" }</span>
                <span class="tool-name">{ &props.tool.name }</span>
                { if foldable { html! {
                    <span class="tool-toggle">
                        { if open { "collapse output" } else { "expand output" } }
                    </span>
                } } else { html!{} } }
            </div>
            { yaml_block(&args) }
            { result_body.unwrap_or_else(|| html!{}) }
        </div>
    }
}

/// An image content block, inline when it is a `data:` URL — the only source
/// the tools and attachment expansion produce. Anything else stays a note: the
/// client does not fetch remote URLs on the transcript's say-so.
fn render_image(source: &str) -> Html {
    if source.starts_with("data:image/") {
        html! { <img class="content-image" src={source.to_string()} alt="image" /> }
    } else {
        html! { <pre class="dim">{ "[image]" }</pre> }
    }
}

/// The `Text` parts of a content run, joined — for bodies that render their
/// `Image` parts as actual images and must not also print an `[image]` note.
fn text_parts(content: &[api::Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            api::Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A person's message, with `@handles` picked out so a room can see who is
/// being spoken to — and so you can spot your own name in a wall of text.
fn render_prose(text: &str, me: Option<&api::Identity>) -> Html {
    let mut out: Vec<Html> = Vec::new();
    let mut plain = String::new();
    for token in text.split_inclusive(char::is_whitespace) {
        let trimmed = token.trim_end();
        let handle = trimmed.strip_prefix('@').map(str::to_lowercase);
        let is_mention = handle
            .as_deref()
            .map(|h| !h.is_empty() && h.chars().all(|c| c.is_alphanumeric() || "_-.".contains(c)))
            .unwrap_or(false);
        if !is_mention {
            plain.push_str(token);
            continue;
        }
        if !plain.is_empty() {
            out.push(html! { { std::mem::take(&mut plain) } });
        }
        let handle = handle.unwrap_or_default();
        let handle = handle.trim_end_matches('.');
        let mine = me
            .map(|i| api::mention::matches_user(handle, &i.id, &i.name))
            .unwrap_or(false);
        let class = if mine {
            "mention mention-me"
        } else {
            "mention"
        };
        out.push(html! { <span {class}>{ trimmed }</span> });
        out.push(html! { { token.strip_prefix(trimmed).unwrap_or("") } });
    }
    if !plain.is_empty() {
        out.push(html! { { plain } });
    }
    html! { <pre>{ for out.into_iter() }</pre> }
}

/// What kind of turn a block of the transcript belongs to.
///
/// Three, by sender — the coarse split a reader actually parses at a glance.
/// Everything a model produced in one stretch is one assistant turn however
/// many content blocks it arrived in; a burst of tool activity is one tool
/// turn however many calls it contains.
#[derive(Clone, PartialEq)]
enum TurnKind {
    /// A person. Carries the author so two people in a row stay two turns.
    User {
        id: String,
        name: String,
    },
    Assistant,
    Tool,
}

impl TurnKind {
    /// Turns merge when this matches: consecutive entries from one sender are
    /// one turn, and a change of sender starts the next.
    fn key(&self) -> String {
        match self {
            TurnKind::User { id, .. } => format!("user:{id}"),
            TurnKind::Assistant => "assistant".into(),
            TurnKind::Tool => "tool".into(),
        }
    }

    fn class(&self) -> &'static str {
        match self {
            TurnKind::User { .. } => "turn turn-user",
            TurnKind::Assistant => "turn turn-assistant",
            TurnKind::Tool => "turn turn-tool",
        }
    }

    fn label(&self) -> String {
        match self {
            TurnKind::User { name, .. } => name.to_uppercase(),
            TurnKind::Assistant => "ASSISTANT".into(),
            TurnKind::Tool => "TOOL".into(),
        }
    }

    fn label_class(&self) -> &'static str {
        match self {
            TurnKind::User { .. } => "role role-user",
            TurnKind::Assistant => "role role-assistant-name",
            TurnKind::Tool => "role role-tool",
        }
    }
}

/// One renderable piece of the transcript, tagged with the turn it belongs to.
///
/// The intermediate that makes grouping possible: entries do not map onto
/// turns one-to-one — a single agent entry can carry prose *and* tool calls,
/// which are two different kinds of turn — so entries are flattened to blocks
/// first and grouped second.
struct Block {
    kind: TurnKind,
    body: Html,
}

/// The turn a person's message belongs to.
fn user_turn(author: &api::Author) -> TurnKind {
    match author {
        api::Author::User { id, name } => TurnKind::User {
            id: id.clone(),
            name: name.clone(),
        },
        // System notes and anything else non-human read as the runtime
        // speaking; group them together under one heading.
        other => TurnKind::User {
            id: "system".into(),
            name: other.name().into(),
        },
    }
}

/// Flatten one entry into the blocks it contributes.
///
/// `results` is the whole transcript's tool results indexed by id, so a call
/// and its output render together even though they are separate entries.
fn blocks_for_entry(
    e: &api::Entry,
    results: &std::collections::HashMap<String, api::ToolResult>,
    verbose: bool,
    me: Option<&api::Identity>,
    out: &mut Vec<Block>,
) {
    match &e.body {
        api::EntryBody::User { content } => {
            let images: Vec<Html> = content
                .iter()
                .filter_map(|c| match c {
                    api::Content::Image { source } => Some(render_image(source)),
                    _ => None,
                })
                .collect();
            out.push(Block {
                kind: user_turn(&e.author),
                body: html! { <>
                    { render_prose(&text_parts(content), me) }
                    { for images.into_iter() }
                </> },
            })
        }
        api::EntryBody::Agent {
            content, tool_uses, ..
        } => {
            for c in content {
                let body = match c {
                    api::Content::Text { text } => {
                        html! { <div class="role-assistant">{ markdown(text) }</div> }
                    }
                    api::Content::Thinking { text, redacted, .. } => html! {
                        <div class="role-thinking">
                            { markdown(if *redacted { "[redacted]" } else { text }) }
                        </div>
                    },
                    api::Content::Image { source } => render_image(source),
                };
                out.push(Block {
                    kind: TurnKind::Assistant,
                    body,
                });
            }
            for t in tool_uses {
                out.push(Block {
                    kind: TurnKind::Tool,
                    // Keyed by the call id so the card that streamed is the
                    // card that persists — an in-place update, not a swap.
                    body: html! {
                        <ToolCard key={t.id.clone()} tool={t.clone()}
                                  result={results.get(&t.id).cloned()} {verbose} />
                    },
                });
            }
        }
        // Folded into the card of the call they answer.
        api::EntryBody::ToolResults { .. } => {}
    }
}

/// The whole transcript, grouped into turns.
///
/// A turn is a `<div>` around a run of blocks from one sender, headed once —
/// so one ASSISTANT heading covers a whole answer however many tool rounds it
/// took, and a run of messages from one person is headed once. There is no
/// separator element between turns: the grouping is structural, and CSS gives
/// it the space and the accent it needs.
///
/// `streaming` is the turn in flight, flattened into the same blocks as
/// anything persisted, so a reply is headed and a tool call is a card while
/// they are still arriving.
fn render_transcript(
    entries: &[api::Entry],
    streaming: &[StreamItem],
    verbose: bool,
    me: Option<&api::Identity>,
) -> Html {
    let results = result_index(entries);
    let mut blocks: Vec<Block> = Vec::new();
    for e in entries {
        blocks_for_entry(e, &results, verbose, me, &mut blocks);
    }
    for item in streaming {
        blocks.push(stream_block(item, verbose));
    }

    // Fold the flat block list into turns.
    let mut turns: Vec<(TurnKind, Vec<Html>)> = Vec::new();
    for b in blocks {
        match turns.last_mut() {
            Some((kind, bodies)) if kind.key() == b.kind.key() => bodies.push(b.body),
            _ => turns.push((b.kind, vec![b.body])),
        }
    }

    html! {
        { for turns.into_iter().map(|(kind, bodies)| html! {
            <div class={kind.class()}>
                <div class={kind.label_class()}>{ kind.label() }</div>
                { for bodies.into_iter() }
            </div>
        }) }
    }
}

/// Index every tool result in the transcript by the call it answers.
fn result_index(entries: &[api::Entry]) -> std::collections::HashMap<String, api::ToolResult> {
    let mut out = std::collections::HashMap::new();
    for e in entries {
        if let api::EntryBody::ToolResults { results } = &e.body {
            for r in results {
                out.insert(r.id.clone(), r.clone());
            }
        }
    }
    out
}

/// Render one streaming item as a transcript block, so live output is grouped
/// into turns by exactly the same code that groups saved output. The tool
/// card is keyed by the call's id — the identity the saved entry carries too,
/// so the card completes and then persists in place instead of shuffling.
fn stream_block(item: &StreamItem, verbose: bool) -> Block {
    match item {
        StreamItem::Text(text) => Block {
            kind: TurnKind::Assistant,
            body: html! { <div class="role-assistant">{ markdown(text) }</div> },
        },
        StreamItem::Thinking(text) => Block {
            kind: TurnKind::Assistant,
            body: html! { <div class="role-thinking">{ markdown(text) }</div> },
        },
        StreamItem::Tool {
            id,
            name,
            input,
            result,
        } => Block {
            kind: TurnKind::Tool,
            body: html! {
                <ToolCard key={id.clone()} tool={api::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }} result={result.clone()} {verbose} />
            },
        },
    }
}

/// Tell the reader a message landed: always a title badge while the tab is in
/// the background, and a desktop notification when it names them.
fn announce(entry: &api::Entry, me: Option<&api::Identity>, unread: &Rc<RefCell<u32>>) {
    let api::EntryBody::User { content } = &entry.body else {
        return;
    };
    let text = api::content_text(content);
    let named = me
        .map(|i| api::mention::addresses_user(&text, &i.id, &i.name))
        .unwrap_or(false);
    if notify::hidden() {
        *unread.borrow_mut() += 1;
        notify::set_unread(*unread.borrow());
    }
    if named || notify::hidden() {
        let who = entry.author.name();
        let title = if named {
            format!("{who} mentioned you")
        } else {
            who.to_string()
        };
        // One notification per session, replaced as messages arrive.
        notify::toast(&title, &text, "myco-session");
    }
}

#[derive(Properties, PartialEq)]
struct ConversationProps {
    id: String,
}

/// The work rail's mutable surface: live shells and their terminal
/// scrollback. A `use_mut_ref` cell (plus a revision counter for renders),
/// because its writers are long-lived tasks and late-completing requests —
/// exactly the closures a captured `UseStateHandle` would go stale in.
#[derive(Default)]
struct PanelBuf {
    shells: Vec<api::Shell>,
    terms: std::collections::HashMap<String, Term>,
}

/// The panel key for one shell: shells are per host, so `(host, id)` is the
/// identity everywhere the GUI files one.
fn term_key(host: &str, id: &str) -> String {
    format!("{host}/{id}")
}

/// One shell's accumulated scrollback view (piped sessions) or latest screen
/// snapshot (pty sessions — see `replace`).
#[derive(Default)]
struct Term {
    /// Absolute offset to tail from next.
    from: u64,
    text: String,
}

/// Keep a terminal's local text bounded; the server ring is the real cap.
const TERM_TEXT_CAP: usize = 128 * 1024;

impl Term {
    fn absorb(&mut self, chunk: &api::ShellTailChunk) {
        if chunk.from > self.from && !self.text.is_empty() {
            self.text.push_str("\n[… output skipped …]\n");
        }
        self.text.push_str(&chunk.data);
        self.from = chunk.end;
        if self.text.len() > TERM_TEXT_CAP {
            let cut = self.text.len() - TERM_TEXT_CAP;
            // Cut on a char boundary; exactness does not matter here.
            let cut = (cut..self.text.len())
                .find(|i| self.text.is_char_boundary(*i))
                .unwrap_or(0);
            self.text.drain(..cut);
        }
    }

    /// Whole-screen replacement for a pty session: the screen snapshot *is*
    /// the view, not an append. Returns whether anything changed.
    fn replace(&mut self, screen: &api::ShellScreen) -> bool {
        if self.text == screen.text {
            return false;
        }
        self.text = screen.text.clone();
        true
    }
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
    // Unread messages that arrived while this tab was in the background.
    let unread: Rc<RefCell<u32>> = use_mut_ref(|| 0u32);
    // The work rail. State the poll task writes lives in refs (see
    // `PanelBuf`); the mirrored `use_state` copies exist only so render-time
    // reads and the toggle button stay ordinary Yew.
    let rail_open = use_state(|| false);
    let rail_open_ref: Rc<RefCell<bool>> = use_mut_ref(|| false);
    // The opened terminal, addressed as shells are: `(host, shell id)`.
    let term_open = use_state(|| Option::<(String, String)>::None);
    let term_open_ref: Rc<RefCell<Option<(String, String)>>> = use_mut_ref(|| None);
    let panel: Rc<RefCell<PanelBuf>> = use_mut_ref(PanelBuf::default);
    let panel_rev = use_state(|| 0u64);
    let term_input = use_node_ref();
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

    // Ask once, on the way in, so the first message that names someone can
    // actually reach them.
    use_effect_with((), |_| notify::request_permission());

    // Coming back to the tab is the acknowledgement: the messages are on
    // screen, so the badge has done its job.
    {
        let unread = unread.clone();
        use_effect_with((), move |_| {
            let on_focus = Closure::<dyn Fn()>::new(move || {
                *unread.borrow_mut() = 0;
                notify::set_unread(0);
            });
            let win = web_sys::window();
            if let Some(w) = &win {
                let _ =
                    w.add_event_listener_with_callback("focus", on_focus.as_ref().unchecked_ref());
            }
            move || {
                if let Some(w) = &win {
                    let _ = w.remove_event_listener_with_callback(
                        "focus",
                        on_focus.as_ref().unchecked_ref(),
                    );
                }
                notify::set_unread(0);
            }
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
        let me = me.clone();
        let unread = unread.clone();
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
                                let mine = matches!(
                                    (&entry.author, me.as_ref()),
                                    (api::Author::User { id, .. }, Some(i)) if *id == i.id
                                );
                                if !mine {
                                    announce(&entry, me.as_ref(), &unread);
                                }
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

    // The work rail's poll: the shells list every tick while the rail is
    // open, plus the opened terminal's scrollback tail. Polling, not SSE —
    // the tail is offset-addressed and idempotent, so a fixed cadence is
    // simple and self-healing, and a closed rail costs nothing.
    {
        let panel = panel.clone();
        let panel_rev = panel_rev.setter();
        let rail_open_ref = rail_open_ref.clone();
        let term_open_ref = term_open_ref.clone();
        let id = props.id.clone();
        use_effect_with(id, move |id| {
            let id = id.clone();
            let alive = Rc::new(std::cell::Cell::new(true));
            let alive2 = alive.clone();
            spawn_local(async move {
                let mut rev = 0u64;
                while alive2.get() {
                    gloo_timers::future::TimeoutFuture::new(1_000).await;
                    if !alive2.get() {
                        break;
                    }
                    if !*rail_open_ref.borrow() {
                        continue;
                    }
                    let mut changed = false;
                    if let Ok(s) =
                        auth::get::<api::Shells>(&format!("/api/sessions/{id}/shells")).await
                    {
                        let mut p = panel.borrow_mut();
                        if p.shells != s.shells {
                            p.shells = s.shells;
                            changed = true;
                        }
                    }
                    let open = term_open_ref.borrow().clone();
                    if let Some((host, shell)) = open {
                        let key = term_key(&host, &shell);
                        // A pty session renders as its current screen (the
                        // snapshot is the view); a piped one accumulates its
                        // scrollback tail.
                        let pty = panel
                            .borrow()
                            .shells
                            .iter()
                            .any(|s| s.host == host && s.id == shell && s.pty);
                        if pty {
                            if let Ok(screen) = auth::get::<api::ShellScreen>(&format!(
                                "/api/sessions/{id}/shells/{host}/{shell}/screen"
                            ))
                            .await
                                && panel
                                    .borrow_mut()
                                    .terms
                                    .entry(key)
                                    .or_default()
                                    .replace(&screen)
                            {
                                changed = true;
                            }
                        } else {
                            let from = panel.borrow().terms.get(&key).map(|t| t.from).unwrap_or(0);
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
                    if changed {
                        rev += 1;
                        panel_rev.set(rev);
                    }
                }
            });
            move || alive.set(false)
        });
    }

    // Keep an open terminal pinned to its bottom as output lands.
    {
        let term_open = (*term_open).clone();
        use_effect_with((*panel_rev, term_open), move |(_, term_open)| {
            if let Some((host, shell)) = term_open
                && let Some(doc) = web_sys::window().and_then(|w| w.document())
                && let Some(el) = doc.get_element_by_id(&format!("term-body-{host}-{shell}"))
            {
                el.set_scroll_top(el.scroll_height());
            }
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

    let select_term = |host: String, shell: String| {
        let term_open = term_open.clone();
        let term_open_ref = term_open_ref.clone();
        Callback::from(move |_: MouseEvent| {
            let this = (host.clone(), shell.clone());
            let next = if term_open.as_ref() == Some(&this) {
                None
            } else {
                Some(this)
            };
            *term_open_ref.borrow_mut() = next.clone();
            term_open.set(next);
        })
    };

    // Take / return the keyboard. The optimistic lock flip makes the border
    // answer the click; the 1s poll is the truth that heals a failed POST.
    let set_lock = |host: String, shell: String, lock: api::ShellLockMode| {
        let id = props.id.clone();
        let panel = panel.clone();
        let panel_rev = panel_rev.clone();
        let dispatch = conv.dispatcher();
        Callback::from(move |_: ()| {
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

    let send_term_input = {
        let id = props.id.clone();
        let term_input = term_input.clone();
        let dispatch = conv.dispatcher();
        move |host: String, shell: String| {
            let id = id.clone();
            let term_input = term_input.clone();
            let dispatch = dispatch.clone();
            Callback::from(move |_: ()| {
                let Some(field) = term_input.cast::<HtmlInputElement>() else {
                    return;
                };
                let data = field.value();
                field.set_value("");
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

    let rail = if *rail_open {
        let p = panel.borrow();
        html! {
            <aside class="rail">
                <h2>{ "running" }</h2>
                { if running.is_empty() { html!{ <div class="empty">{ "nothing in flight" }</div> } }
                  else { html! { for running.iter().map(|(call_id, name)| html! {
                    <div class="rail-tool" onclick={jump_to_card(call_id.clone())}>
                        <span class="tool-disc tool-disc-running">{ "●" }</span>
                        <span class="tool-name">{ name }</span>
                    </div>
                  }) } } }
                <h2>{ "shells" }</h2>
                { if p.shells.is_empty() { html!{ <div class="empty">{ "no live shells" }</div> } }
                  else { html! { for p.shells.iter().map(|shell| {
                    let opened = term_open.as_ref()
                        .is_some_and(|(h, s)| *h == shell.host && *s == shell.id);
                    let user_locked = shell.lock == api::ShellLockMode::User;
                    let head = html! {
                        <div class="term-head" onclick={select_term(shell.host.clone(), shell.id.clone())}
                             ondblclick={ let take = set_lock(shell.host.clone(), shell.id.clone(), api::ShellLockMode::User);
                                          Callback::from(move |_: MouseEvent| take.emit(())) }>
                            <span class={ if shell.running { "tool-disc tool-disc-running" }
                                          else { "tool-disc tool-disc-ok" } }>{ "●" }</span>
                            <span class="tool-name">{ &shell.id }</span>
                            { if shell.host != "local" { html! {
                                <span class="host-badge">{ format!("@{}", shell.host) }</span>
                            } } else { html!{} } }
                            <span class="dim">{ &shell.cmdline }</span>
                            <span class="lock-badge">
                                { if user_locked { "⌨ yours" } else { "⌨ agent" } }
                            </span>
                        </div>
                    };
                    if !opened {
                        return html! { <div class={ classes!("term", user_locked.then_some("term-user")) }>{ head }</div> };
                    }
                    let text = p.terms.get(&term_key(&shell.host, &shell.id))
                        .map(|t| t.text.clone()).unwrap_or_default();
                    let on_key = {
                        let send = send_term_input(shell.host.clone(), shell.id.clone());
                        let release = set_lock(shell.host.clone(), shell.id.clone(), api::ShellLockMode::Assistant);
                        Callback::from(move |e: KeyboardEvent| {
                            if e.key() == "Enter" {
                                e.prevent_default();
                                send.emit(());
                            } else if e.key() == "Escape" {
                                e.prevent_default();
                                release.emit(());
                            }
                        })
                    };
                    html! {
                        <div class={ classes!("term", user_locked.then_some("term-user")) }>
                            { head }
                            <pre id={format!("term-body-{}-{}", shell.host, shell.id)} class="term-body">{ text }</pre>
                            { if user_locked { html! {
                                <div class="term-input-row">
                                    <input ref={term_input.clone()} type="text"
                                           placeholder="type into the shell (Enter sends · Esc hands the keyboard back)"
                                           onkeydown={on_key} />
                                </div>
                            } } else { html! {
                                <div class="term-hint">{ "double-click the title to take the keyboard" }</div>
                            } } }
                        </div>
                    }
                  }) } } }
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
                { render_transcript(&st.entries, &st.streaming, *verbose,
                                    who.as_ref().map(|s| &s.identity)) }
                { if let Some(e) = &st.error { html! {
                    <div class="err"><hr class="rule" /><b>{ "ERROR" }</b><pre>{ e }</pre></div>
                } } else { html!{} } }
                </div>
            </div>
            { rail }
            </div>
            <div class="composer">
                <div class="column">
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
    }
}
