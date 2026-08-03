//! Minimal Yew client for the myco API server: a session browser at `/` and
//! one conversation per URL at `/session/<id>`. Live output arrives over SSE;
//! a slow poll reconciles the transcript as a fallback.
//!
//! The visual identity is the terminal's: monospace, dark, USER/ASSISTANT
//! section rules — the web page renders (approximately) what the CLI prints,
//! with no further chrome.

mod auth;
mod highlight;

use std::cell::RefCell;
use std::rc::Rc;

use futures::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use myco_api as api;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlInputElement, HtmlTextAreaElement};
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
            auth::clear_token();
            identity.set(None);
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

/// Token entry. Deliberately the whole page and nothing else: there is no
/// anonymous view of this server to decorate.
#[function_component(Login)]
fn login(props: &LoginProps) -> Html {
    let input = use_node_ref();
    let error = use_state(|| Option::<String>::None);

    let submit: Callback<()> = {
        let input = input.clone();
        let error = error.clone();
        let on_signed_in = props.on_signed_in.clone();
        Callback::from(move |_| {
            let Some(el) = input.cast::<HtmlInputElement>() else {
                return;
            };
            let value = el.value();
            if value.trim().is_empty() {
                return;
            }
            auth::set_token(&value);
            let error = error.clone();
            let on_signed_in = on_signed_in.clone();
            spawn_local(async move {
                match auth::whoami().await {
                    Ok(who) => {
                        error.set(None);
                        on_signed_in.emit(who);
                    }
                    Err(Failure::Unauthorized) => {
                        auth::clear_token();
                        error.set(Some("that token is not in the roster".into()));
                    }
                    Err(e) => error.set(Some(e.to_string())),
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
                <div class="column">
                    <h1>{ "myco" }</h1>
                    <p class="dim">
                        { "Paste the token for your entry in " }
                        <code>{ "server.toml" }</code>{ "." }
                    </p>
                    <input ref={input} type="password" onkeydown={on_keydown}
                           placeholder="token" />
                    { " " }
                    <button onclick={on_click}>{ "sign in" }</button>
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
fn json_block(pretty: &str) -> Html {
    let body = highlight::json_to_html(pretty);
    Html::from_html_unchecked(AttrValue::from(format!(
        "<pre class=\"code json\">{body}</pre>"
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
                                        title: None,
                                        archived: Some(!archived),
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

    let send_now: Callback<()> = {
        let input = input.clone();
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
            let error = error.clone();
            let navigator = navigator.clone();
            spawn_local(async move {
                // Create, then post, then hand the URL over to `Conversation`.
                let created = auth::post_json::<api::SessionSummary, _>(
                    "/api/sessions",
                    &api::CreateSession {
                        model: None,
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

/// One tool call as its own bordered block: name, pretty-printed arguments,
/// and the result folded in underneath.
///
/// Collapsed, strings in the arguments are truncated and the result is capped
/// — the same policy the CLI applies, shared from `myco_api` so the two
/// frontends cannot drift. Expanded, nothing is elided.
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

    let args = api::tool_input_json(&props.tool.input, !open);
    let is_error = props.result.as_ref().is_some_and(|r| r.is_error);

    let result_body = props.result.as_ref().map(|r| {
        let text = api::content_text(&r.content);
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
                <pre class={ if r.is_error { "err" } else { "dim" } }>{ shown }</pre>
                { if !open && hidden > 0 { html! {
                    <button class="linkish" onclick={toggle.clone()}>
                        { format!("+{hidden} more lines") }
                    </button>
                } } else { html!{} } }
            </div>
        }
    });

    html! {
        <div class={ classes!("tool-card", is_error.then_some("tool-card-error")) }>
            <div class="tool-head" onclick={toggle.clone()}>
                <span class="tool-caret">{ if open { "▾" } else { "▸" } }</span>
                <span class="tool-name">{ &props.tool.name }</span>
                { if props.result.is_none() { html! {
                    <span class="dim">{ " · running" }</span>
                } } else if is_error { html! {
                    <span class="err">{ " · error" }</span>
                } } else { html!{} } }
                <span class="tool-toggle">{ if open { "collapse" } else { "expand" } }</span>
            </div>
            { json_block(&args) }
            { result_body.unwrap_or_else(|| html!{}) }
        </div>
    }
}

/// One transcript entry, in the terminal's visual language. Prose is
/// markdown-rendered (as the CLI styles it); tool calls become cards.
/// Entries carry their author, so a shared session reads as a conversation
/// rather than an anonymous stream.
///
/// `results` is the whole transcript's tool results indexed by id, so a call
/// and its output render together even though they are separate entries.
fn render_entry(
    e: &api::Entry,
    results: &std::collections::HashMap<String, api::ToolResult>,
    verbose: bool,
) -> Html {
    match &e.body {
        api::EntryBody::User { content } => html! {
            <div>
                <hr class="rule" />
                <div class="role role-user">{ e.author.name().to_uppercase() }</div>
                <pre>{ api::content_text(content) }</pre>
            </div>
        },
        api::EntryBody::Agent {
            content, tool_uses, ..
        } => html! {
            <div>
                { for content.iter().map(|c| match c {
                    api::Content::Text { text } => html! {
                        <div class="role-assistant">{ markdown(text) }</div>
                    },
                    api::Content::Thinking { text, redacted, .. } => html! {
                        <div class="role-thinking">
                            { markdown(if *redacted { "[redacted]" } else { text }) }
                        </div>
                    },
                    api::Content::Image { .. } => html! { <pre class="dim">{ "[image]" }</pre> },
                }) }
                { for tool_uses.iter().map(|t| html! {
                    <ToolCard tool={t.clone()} result={results.get(&t.id).cloned()}
                              {verbose} />
                }) }
            </div>
        },
        // Folded into the card of the call they answer.
        api::EntryBody::ToolResults { .. } => html! {},
    }
}

/// The whole transcript. Results are indexed once for the pass, so a call
/// and its output render together even though they are separate entries.
fn render_transcript(entries: &[api::Entry], verbose: bool) -> Html {
    let results = result_index(entries);
    html! { for entries.iter().map(|e| render_entry(e, &results, verbose)) }
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

#[derive(Properties, PartialEq)]
struct ConversationProps {
    id: String,
}

#[function_component(Conversation)]
fn conversation(props: &ConversationProps) -> Html {
    let who = use_context::<Session>();
    let entries = use_state(Vec::<api::Entry>::new);
    let verbose = use_state(|| false);
    let busy = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    // Text streamed since the last transcript refresh. The buffer is a ref,
    // not state: a task spawned once would otherwise keep reading the value
    // captured at its render andeach delta would replace rather than append.
    let stream_buf: Rc<RefCell<String>> = use_mut_ref(String::new);
    let streaming = use_state(String::new);
    let input = use_node_ref();
    let pane = use_node_ref();
    let at_bottom: Rc<RefCell<bool>> = use_mut_ref(|| true);
    let navigator = use_navigator().unwrap();

    // SSE: live deltas; turn boundaries refresh the transcript.
    {
        let entries = entries.clone();
        let busy = busy.clone();
        let error = error.clone();
        let streaming = streaming.clone();
        let stream_buf = stream_buf.clone();
        let navigator = navigator.clone();
        let id = props.id.clone();
        use_effect_with(id, move |id| {
            let id = id.clone();
            let alive = Rc::new(std::cell::Cell::new(true));
            let alive2 = alive.clone();
            spawn_local(async move {
                // Existing transcript first, so opening a session shows its
                // history whether or not the live feed ever connects.
                match fetch_detail(&id).await {
                    Ok(d) => {
                        entries.set(d.entries);
                        busy.set(d.summary.busy);
                    }
                    Err(e) => error.set(Some(e.to_string())),
                }

                let es = match EventSource::new(&auth::sse_url(&id)) {
                    Ok(es) => es,
                    Err(e) => {
                        error.set(Some(format!("event stream: {e}")));
                        return;
                    }
                };
                let mut es = es;
                let Ok(mut stream) = es.subscribe("message") else {
                    error.set(Some("event stream: cannot subscribe".into()));
                    return;
                };
                while alive2.get() {
                    let Some(Ok((_, msg))) = stream.next().await else {
                        break;
                    };
                    let Some(data) = msg.data().as_string() else {
                        continue;
                    };
                    let Ok(ev) = serde_json::from_str::<api::StreamEvent>(&data) else {
                        continue;
                    };
                    let push = |text: &str| {
                        stream_buf.borrow_mut().push_str(text);
                        streaming.set(stream_buf.borrow().clone());
                    };
                    match ev {
                        api::StreamEvent::TurnStarted => {
                            busy.set(true);
                            error.set(None);
                            stream_buf.borrow_mut().clear();
                            streaming.set(String::new());
                            if let Ok(p) = fetch_poll(&id).await {
                                entries.set(p.entries);
                            }
                        }
                        api::StreamEvent::TextDelta { text }
                        | api::StreamEvent::ThinkingDelta { text } => push(&text),
                        api::StreamEvent::ToolStarted { name, .. } => {
                            push(&format!("\n● {name}\n"))
                        }
                        api::StreamEvent::TurnFailed { message } => error.set(Some(message)),
                        api::StreamEvent::Compacted { successor, .. } => {
                            navigator.push(&Route::Session { id: successor });
                            break;
                        }
                        api::StreamEvent::TurnFinished => {
                            stream_buf.borrow_mut().clear();
                            streaming.set(String::new());
                            match fetch_poll(&id).await {
                                Ok(p) => {
                                    entries.set(p.entries);
                                    busy.set(p.busy);
                                    if p.last_error.is_some() {
                                        error.set(p.last_error);
                                    }
                                }
                                Err(e) => error.set(Some(e.to_string())),
                            }
                        }
                    }
                }
                // Dropping `es` closes the connection.
                drop(es);
            });
            move || alive.set(false)
        });
    }

    // Slow reconciliation poll: covers stream hiccups and other writers.
    {
        let entries = entries.clone();
        let busy = busy.clone();
        let id = props.id.clone();
        use_effect_with(id, move |id| {
            let id = id.clone();
            let alive = Rc::new(std::cell::Cell::new(true));
            let alive2 = alive.clone();
            spawn_local(async move {
                while alive2.get() {
                    gloo_timers::future::TimeoutFuture::new(5_000).await;
                    if let Ok(p) = fetch_poll(&id).await {
                        entries.set(p.entries);
                        busy.set(p.busy);
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
        use_effect_with((entries.len(), streaming.len()), move |_| {
            if *at_bottom.borrow()
                && let Some(el) = pane.cast::<web_sys::Element>()
            {
                el.set_scroll_top(el.scroll_height());
            }
        });
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
        let busy = busy.clone();
        let error = error.clone();
        Callback::from(move |_| {
            let Some(ta) = input.cast::<HtmlTextAreaElement>() else {
                return;
            };
            let text = ta.value();
            if text.trim().is_empty() {
                return;
            }
            ta.set_value("");
            busy.set(true);
            let id = id.clone();
            let error = error.clone();
            spawn_local(async move {
                if let Err(e) = auth::post_json::<api::Poll, _>(
                    &format!("/api/sessions/{id}/messages"),
                    &api::PostMessage { text },
                )
                .await
                {
                    error.set(Some(format!("send failed: {e}")));
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

    let on_cancel = {
        let id = props.id.clone();
        Callback::from(move |_: MouseEvent| {
            let id = id.clone();
            spawn_local(async move {
                let _ = auth::post::<api::Poll>(&format!("/api/sessions/{id}/cancel")).await;
            });
        })
    };

    html! {
        <div class="app">
            <div class="topbar">
                <div class="column">
                    <Link<Route> to={Route::Browser}>{ "← sessions" }</Link<Route>>
                    <span class="dim">{ format!(" {} ", props.id) }</span>
                    { if *busy { html!{ <span class="dim">{ "· working…" }</span> } } else { html!{} } }
                    <button class="linkish" onclick={toggle_verbose}>
                        { if *verbose { "· concise" } else { "· verbose" } }
                    </button>
                    { whoami_line(&who) }
                </div>
            </div>
            <div class="pane" ref={pane} onscroll={on_scroll}>
                <div class="column">
                { render_transcript(&entries, *verbose) }
                { if !streaming.is_empty() {
                    html! { <div class="role-assistant">{ markdown(&streaming) }</div> }
                } else { html!{} } }
                { if let Some(e) = &*error { html! {
                    <div class="err"><hr class="rule" /><b>{ "ERROR" }</b><pre>{ e }</pre></div>
                } } else { html!{} } }
                </div>
            </div>
            <div class="composer">
                <div class="column">
                    <textarea ref={input} rows="3" onkeydown={on_keydown}
                              placeholder="message (Enter to send, Shift+Enter for a newline)" />
                    <div class="actions">
                        <button class="send" onclick={on_send}>{ "send" }</button>
                        { if *busy {
                            html! { <button onclick={on_cancel}>{ "cancel" }</button> }
                        } else { html!{} } }
                    </div>
                </div>
            </div>
        </div>
    }
}
