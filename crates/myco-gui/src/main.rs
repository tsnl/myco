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
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

    let mut events = Vec::new();
    // `Some` while inside a fence: the language and the code collected so far.
    let mut fence: Option<(String, String)> = None;
    // Tables and strikethrough are extensions the parser must be asked for —
    // without them a model's table renders as one line of pipes.
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for ev in Parser::new_ext(src, options) {
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

/// Lines of a collapsed call's *arguments* before they are cut off. More
/// generous than the result budget — the arguments are what you scan a
/// transcript for — but a 200-line heredoc is still a click away, not a
/// wall.
const ARGS_PREVIEW_LINES: usize = 15;

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
/// The call and its output are collapsed on different budgets, because they
/// are read for different reasons. The arguments are *what was asked for* —
/// the thing you scan a transcript to find — so they get the larger cap; the
/// output is *what came back*, which can be a 5,000-line build log, so it is
/// cut sooner. One toggle opens both in full.
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

    // The whole call, structurally: every argument appears, long string
    // values elided. How it reads is the tool's own business — `bash` lays
    // its command out as shell, everything else falls back to the structural
    // rendering. Line-capped like the result (a 300-line heredoc command
    // would bury the conversation), with its own expander.
    let args = api::tool_display::tool_input_yaml(
        &props.tool.name,
        &props.tool.input,
        api::tool_display::TOOL_DISPLAY_WIDTH,
    );
    let args_total = args.lines().count();
    let args_shown = if open || args_total <= ARGS_PREVIEW_LINES {
        args.clone()
    } else {
        args.lines()
            .take(ARGS_PREVIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let args_hidden = args_total.saturating_sub(ARGS_PREVIEW_LINES);
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

    // Nothing to fold away unless something got cut: a long result, or a
    // long call.
    let foldable = args_hidden > 0
        || props
            .result
            .as_ref()
            .is_some_and(|r| text_parts(&r.content).lines().count() > RESULT_PREVIEW_LINES);

    // The DOM id is the jump target for the work panel's running-tool chips.
    html! {
        <div id={format!("tool-{}", props.tool.id)}
             class={ classes!("tool-card", is_error.then_some("tool-card-error")) }>
            <div class="tool-head" onclick={toggle.clone()}>
                <span class={status.disc_class()} title={status.title()}>{ "●" }</span>
                <span class="tool-name">{ &props.tool.name }</span>
                { if foldable { html! {
                    <span class="tool-toggle">
                        { if open { "collapse" } else { "expand" } }
                    </span>
                } } else { html!{} } }
            </div>
            { yaml_block(&args_shown) }
            { if !open && args_hidden > 0 { html! {
                <button class="linkish tool-args-more" onclick={toggle.clone()}>
                    { format!("+{args_hidden} more lines of the call") }
                </button>
            } } else { html!{} } }
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
    arrivals: &[api::Entry],
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
    // Messages delivered mid-turn come after the stream: that is where the
    // room actually saw them.
    for e in arrivals {
        blocks_for_entry(e, &results, verbose, me, &mut blocks);
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

#[derive(Properties, PartialEq)]
struct ConversationProps {
    id: String,
}

/// The work panel's mutable surface: live shells and subagent children, the
/// opened terminals' scrollback, and the opened chats' transcripts. A
/// `use_mut_ref` cell (plus a revision counter for renders), because its
/// writers are long-lived tasks and late-completing requests — exactly the
/// closures a captured `UseStateHandle` would go stale in.
#[derive(Default)]
struct PanelBuf {
    shells: Vec<api::Shell>,
    terms: std::collections::HashMap<String, Term>,
    /// Each pty session's latest rendered screen (the snapshot is the view).
    screens: std::collections::HashMap<String, api::ShellScreen>,
    /// Live subagent children — the `subagent` tool's sessions, as tabs.
    subs: Vec<api::Subagent>,
    /// Each opened child's transcript so far (`poll?since=` pages).
    chats: std::collections::HashMap<String, Chat>,
}

/// Keystrokes on their way to one shell: a byte buffer drained by a single
/// in-flight POST at a time, so bytes arrive in the order they were typed
/// however fast the typist outruns the network.
#[derive(Default)]
struct TypeQueue {
    buf: String,
    busy: bool,
}

/// One subagent chat's accumulated entries.
#[derive(Default)]
struct Chat {
    since: usize,
    entries: Vec<api::Entry>,
}

/// One tab of the work panel: a shell terminal or a subagent chat.
#[derive(Clone, PartialEq)]
enum Tab {
    Shell { host: String, id: String },
    Sub { id: String },
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
}

/// Translate one keydown into the bytes a terminal sends, or `None` for
/// keys the browser should keep. Follows xterm's conventions, including the
/// copy/paste carve-out: `Ctrl+Shift+C`/`Ctrl+Shift+V` stay the browser's,
/// while plain `Ctrl+C`/`Ctrl+V` are the control bytes a terminal owns.
fn key_bytes(e: &KeyboardEvent, app_cursor: bool) -> Option<String> {
    if e.meta_key() {
        return None;
    }
    let key = e.key();
    if e.ctrl_key() && e.shift_key() && (key == "C" || key == "V") {
        return None;
    }
    if e.ctrl_key() && !e.alt_key() {
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let byte = match c {
                'a'..='z' => (c as u8) - b'a' + 1,
                'A'..='Z' => (c as u8) - b'A' + 1,
                '@' | ' ' => 0,
                '[' => 0x1b,
                '\\' => 0x1c,
                ']' => 0x1d,
                '^' => 0x1e,
                '_' => 0x1f,
                _ => return None,
            };
            return Some(char::from(byte).to_string());
        }
        return None;
    }
    let arrow = |c: char| {
        Some(if app_cursor {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        })
    };
    let base = match key.as_str() {
        "Enter" => Some("\r".to_string()),
        "Backspace" => Some("\x7f".to_string()),
        "Tab" => Some("\t".to_string()),
        "Escape" => Some("\x1b".to_string()),
        "ArrowUp" => arrow('A'),
        "ArrowDown" => arrow('B'),
        "ArrowRight" => arrow('C'),
        "ArrowLeft" => arrow('D'),
        "Home" => Some("\x1b[H".to_string()),
        "End" => Some("\x1b[F".to_string()),
        "PageUp" => Some("\x1b[5~".to_string()),
        "PageDown" => Some("\x1b[6~".to_string()),
        "Insert" => Some("\x1b[2~".to_string()),
        "Delete" => Some("\x1b[3~".to_string()),
        "F1" => Some("\x1bOP".to_string()),
        "F2" => Some("\x1bOQ".to_string()),
        "F3" => Some("\x1bOR".to_string()),
        "F4" => Some("\x1bOS".to_string()),
        "F5" => Some("\x1b[15~".to_string()),
        "F6" => Some("\x1b[17~".to_string()),
        "F7" => Some("\x1b[18~".to_string()),
        "F8" => Some("\x1b[19~".to_string()),
        "F9" => Some("\x1b[20~".to_string()),
        "F10" => Some("\x1b[21~".to_string()),
        "F11" => Some("\x1b[23~".to_string()),
        "F12" => Some("\x1b[24~".to_string()),
        k if k.chars().count() == 1 => Some(k.to_string()),
        _ => None,
    };
    match (base, e.alt_key()) {
        // Alt is the ESC prefix, terminal-style.
        (Some(b), true) => Some(format!("\x1b{b}")),
        (b, false) => b,
        (None, true) => None,
    }
}

/// The styled terminal grid: one `div` per row, one `span` per same-styled
/// run, colors resolved server-side. The cursor cell renders inverse.
fn render_screen(screen: &api::ShellScreen) -> Html {
    let mut runs = screen.runs.iter().peekable();
    let rows = (0..screen.rows).map(|row| {
        let mut spans: Vec<Html> = Vec::new();
        while let Some(r) = runs.peek() {
            if r.row != row {
                break;
            }
            let r = runs.next().expect("peeked");
            // Inverse swaps the pair; a missing side swaps in the terminal
            // default. The cursor is its own run and styles itself.
            let (fg, bg) = if r.inverse && !r.cursor {
                (
                    Some(r.bg.clone().unwrap_or_else(|| "var(--term-bg)".into())),
                    Some(r.fg.clone().unwrap_or_else(|| "var(--fg)".into())),
                )
            } else {
                (r.fg.clone(), r.bg.clone())
            };
            let mut style = String::new();
            if !r.cursor {
                if let Some(fg) = fg {
                    style.push_str(&format!("color:{fg};"));
                }
                if let Some(bg) = bg {
                    style.push_str(&format!("background:{bg};"));
                }
            }
            if r.bold {
                style.push_str("font-weight:bold;");
            }
            if r.italic {
                style.push_str("font-style:italic;");
            }
            if r.underline {
                style.push_str("text-decoration:underline;");
            }
            spans.push(html! {
                <span class={ classes!(r.cursor.then_some("cur")) } style={style}>
                    { r.text.clone() }
                </span>
            });
        }
        html! { <div class="trow">{ for spans.into_iter() }</div> }
    });
    html! { <div class="term-screen">{ for rows }</div> }
}

/// The active tab's window — a shell terminal or a subagent chat behind one
/// piece of chrome. The head names the thing and carries the keyboard-lock
/// affordance as an explicit button (a double-click on a header that also
/// selects was two gestures fighting over one element).
///
/// Two input modes, by what the thing *is*:
/// - a pty session is a real terminal: while the user holds the lock the
///   body itself has keyboard focus and every keystroke goes down raw —
///   Ctrl+C, Esc, arrows, F-keys, Alt chords, paste — so full-screen
///   programs behave. The window is also fitted to the panel (resize →
///   SIGWINCH).
/// - a piped shell or a subagent chat is line-based: an input row appears
///   while the lock is held (Enter sends, Esc hands back).
#[derive(Properties, PartialEq)]
struct WorkViewProps {
    /// Something is running inside (a command, a turn) — the disc state.
    running: bool,
    title: String,
    /// Small annotation after the title: `@host`, a model key.
    #[prop_or_default]
    badge: Option<String>,
    /// Dim detail text in the head (a shell's command line).
    #[prop_or_default]
    detail: String,
    user_locked: bool,
    /// Prose content (a chat) rather than preformatted terminal text.
    #[prop_or_default]
    chat: bool,
    /// A pty session: raw keystrokes into the body, no input row.
    #[prop_or_default]
    pty: bool,
    /// DECCKM from the screen: arrows send SS3 instead of CSI.
    #[prop_or_default]
    app_cursor: bool,
    /// Point the keyboard: `true` takes it, `false` hands it back.
    on_lock: Callback<bool>,
    /// Rename affordance (shells only; a subagent's name is its id).
    #[prop_or_default]
    on_rename: Option<Callback<()>>,
    /// A line submitted from the input row (line mode).
    #[prop_or_default]
    on_send: Callback<String>,
    /// Raw bytes typed or pasted into the terminal (pty mode).
    #[prop_or_default]
    on_keys: Callback<String>,
    /// The cols×rows that currently fit the body (pty mode, lock held).
    #[prop_or_default]
    on_resize: Callback<(u16, u16)>,
    #[prop_or_default]
    placeholder: AttrValue,
    #[prop_or_default]
    children: Html,
}

#[function_component(WorkView)]
fn work_view(props: &WorkViewProps) -> Html {
    let input = use_node_ref();
    let body = use_node_ref();
    let probe = use_node_ref();
    let raw_mode = props.pty && props.user_locked;

    // Ride the bottom on every render — for the scrolling views. A pty
    // screen is a fixed grid; there is nothing to chase.
    {
        let body = body.clone();
        let scrolling = !props.pty;
        use_effect(move || {
            if scrolling && let Some(el) = body.cast::<web_sys::Element>() {
                el.set_scroll_top(el.scroll_height());
            }
        });
    }

    // Taking a terminal's keyboard focuses it, so typing starts immediately.
    {
        let body = body.clone();
        use_effect_with(raw_mode, move |raw| {
            if *raw && let Some(el) = body.cast::<web_sys::HtmlElement>() {
                let _ = el.focus();
            }
        });
    }

    // Fit the terminal to the panel: measure a 10-char probe for the cell
    // size, emit the cols×rows that fit. The parent dedupes and resizes the
    // pty; runs every render so panel-size changes are picked up on the
    // next tick.
    {
        let body = body.clone();
        let probe = probe.clone();
        let on_resize = props.on_resize.clone();
        use_effect(move || {
            if raw_mode
                && let Some(p) = probe.cast::<web_sys::HtmlElement>()
                && let Some(b) = body.cast::<web_sys::HtmlElement>()
            {
                let cell_w = p.offset_width() as f64 / 10.0;
                let cell_h = p.offset_height() as f64;
                if cell_w > 0.0 && cell_h > 0.0 {
                    let cols = ((b.client_width() as f64 - 18.0) / cell_w) as i64;
                    let rows = ((b.client_height() as f64 - 10.0) / cell_h) as i64;
                    if cols >= 10 && rows >= 3 {
                        on_resize.emit((cols.min(500) as u16, rows.min(500) as u16));
                    }
                }
            }
        });
    }

    let on_lock_click = {
        let on_lock = props.on_lock.clone();
        let take = !props.user_locked;
        Callback::from(move |_: MouseEvent| on_lock.emit(take))
    };
    // Line mode: Enter sends the line, Esc hands the keyboard back.
    let on_line_key = {
        let input = input.clone();
        let on_send = props.on_send.clone();
        let on_lock = props.on_lock.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                if let Some(field) = input.cast::<HtmlInputElement>() {
                    let text = field.value();
                    field.set_value("");
                    on_send.emit(text);
                }
            } else if e.key() == "Escape" {
                e.prevent_default();
                on_lock.emit(false);
            }
        })
    };
    // Raw mode: the body is the terminal. (Esc types ESC here — handing the
    // keyboard back is the head button.)
    let on_raw_key = {
        let on_keys = props.on_keys.clone();
        let app_cursor = props.app_cursor;
        Callback::from(move |e: KeyboardEvent| {
            if let Some(bytes) = key_bytes(&e, app_cursor) {
                e.prevent_default();
                on_keys.emit(bytes);
            }
        })
    };
    let on_paste = {
        let on_keys = props.on_keys.clone();
        Callback::from(move |e: Event| {
            use wasm_bindgen::JsCast;
            let Ok(e) = e.dyn_into::<web_sys::ClipboardEvent>() else {
                return;
            };
            if let Some(data) = e.clipboard_data()
                && let Ok(text) = data.get_data("text")
                && !text.is_empty()
            {
                e.prevent_default();
                on_keys.emit(text);
            }
        })
    };

    html! {
        <div class={ classes!("work-view", props.user_locked.then_some("work-view-user")) }>
            <div class="work-view-head">
                <span class={ if props.running { "tool-disc tool-disc-running" }
                              else { "tool-disc tool-disc-ok" } }>{ "●" }</span>
                <span class="tool-name">{ &props.title }</span>
                { if let Some(b) = &props.badge { html! {
                    <span class="host-badge">{ b }</span>
                } } else { html!{} } }
                <span class="dim">{ &props.detail }</span>
                { if let Some(rename) = props.on_rename.clone() { html! {
                    <button class="linkish rename-badge" title="rename this terminal"
                            onclick={Callback::from(move |_: MouseEvent| rename.emit(()))}>
                        { "✎" }
                    </button>
                } } else { html!{} } }
                <button class="linkish lock-badge" onclick={on_lock_click}>
                    { if props.user_locked { "⌨ yours — hand back" } else { "⌨ agent — take keyboard" } }
                </button>
            </div>
            <div ref={body}
                 class={ classes!("work-body", props.chat.then_some("chat-body"),
                                  raw_mode.then_some("work-body-raw")) }
                 tabindex={ raw_mode.then_some("0") }
                 onkeydown={ raw_mode.then_some(on_raw_key) }
                 onpaste={ raw_mode.then_some(on_paste) }>
                <span class="term-probe" ref={probe} aria-hidden="true">{ "0000000000" }</span>
                { props.children.clone() }
            </div>
            { if props.user_locked && !props.pty { html! {
                <div class="term-input-row">
                    <input ref={input} type="text" placeholder={props.placeholder.clone()}
                           onkeydown={on_line_key} />
                </div>
            } } else { html!{} } }
        </div>
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
