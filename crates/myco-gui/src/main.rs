//! Minimal Yew client for the myco API server: a session browser at `/` and
//! one conversation per URL at `/session/<id>`. Live output arrives over SSE;
//! a slow poll reconciles the transcript as a fallback.
//!
//! The visual identity is the terminal's: monospace, dark, USER/ASSISTANT
//! section rules — the web page renders (approximately) what the CLI prints,
//! with no further chrome.

use std::cell::RefCell;
use std::rc::Rc;

use futures::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use gloo_net::http::Request;
use myco_api as api;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlTextAreaElement;
use yew::prelude::*;
use yew_router::prelude::*;

#[derive(Clone, Routable, PartialEq)]
enum Route {
    #[at("/")]
    Browser,
    #[at("/session/:id")]
    Session { id: String },
}

fn main() {
    yew::Renderer::<App>::new().render();
}

#[function_component(App)]
fn app() -> Html {
    html! {
        <BrowserRouter>
            <Switch<Route> render={|route| match route {
                Route::Browser => html! { <Browser /> },
                Route::Session { id } => html! { <Conversation {id} /> },
            }} />
        </BrowserRouter>
    }
}

/// GET a JSON endpoint, carrying failures as text: a silently empty
/// transcript is indistinguishable from a broken one, so nothing here
/// swallows an error.
async fn fetch<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let resp = Request::get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.ok() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<api::ApiError>(&body)
            .map(|e| e.error)
            .unwrap_or(body);
        return Err(format!("GET {url}: http {status}: {detail}"));
    }
    resp.json::<T>()
        .await
        .map_err(|e| format!("GET {url}: decode: {e}"))
}

async fn fetch_detail(id: &str) -> Result<api::SessionDetail, String> {
    fetch(&format!("/api/sessions/{id}")).await
}

async fn fetch_poll(id: &str) -> Result<api::Poll, String> {
    fetch(&format!("/api/sessions/{id}/poll?since=0")).await
}

/// Markdown → HTML, with raw HTML in the source neutralized (a model must
/// not be able to inject markup into the page).
fn markdown(src: &str) -> Html {
    use pulldown_cmark::{Event, Parser};
    let events = Parser::new(src).map(|e| match e {
        Event::Html(t) | Event::InlineHtml(t) => Event::Text(t),
        other => other,
    });
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events);
    Html::from_html_unchecked(AttrValue::from(format!("<div class=\"md\">{out}</div>")))
}

// ---------------------------------------------------------------------------
// Session browser (top level)
// ---------------------------------------------------------------------------

#[function_component(Browser)]
fn browser() -> Html {
    let sessions = use_state(Vec::<api::SessionSummary>::new);
    let navigator = use_navigator().unwrap();

    {
        let sessions = sessions.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                if let Ok(resp) = Request::get("/api/sessions").send().await
                    && let Ok(list) = resp.json::<Vec<api::SessionSummary>>().await
                {
                    sessions.set(list);
                }
            });
        });
    }

    let on_new = {
        let navigator = navigator.clone();
        Callback::from(move |_| {
            let navigator = navigator.clone();
            spawn_local(async move {
                let req = Request::post("/api/sessions")
                    .json(&api::CreateSession {
                        model: None,
                        parent_session: None,
                        fork: false,
                    })
                    .unwrap();
                if let Ok(resp) = req.send().await
                    && let Ok(s) = resp.json::<api::SessionSummary>().await
                {
                    navigator.push(&Route::Session { id: s.id });
                }
            });
        })
    };

    html! {
        <div class="browser">
            <div class="column">
            <h1>{ "myco" }</h1>
            <button onclick={on_new}>{ "new session" }</button>
            <ul>
                { for sessions.iter().map(|s| {
                    let title = s.title.clone().unwrap_or_else(|| s.snippet.clone());
                    let label = format!(
                        "{} — {} [{}{}] {}",
                        &s.id[..s.id.len().min(8)],
                        s.model,
                        if s.live { "live" } else { "idle" },
                        if s.busy { ", busy" } else { "" },
                        title,
                    );
                    html! {
                        <li>
                            <Link<Route> to={Route::Session { id: s.id.clone() }}>{ label }</Link<Route>>
                        </li>
                    }
                }) }
            </ul>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Conversation (one URL per session)
// ---------------------------------------------------------------------------

/// One transcript entry, in the terminal's visual language. Prose is
/// markdown-rendered (as the CLI styles it); the user's own words and raw
/// tool output stay verbatim, exactly as the terminal replays them.
fn render_entry(e: &api::Entry) -> Html {
    match e.role.as_str() {
        "user" => html! {
            <div>
                <hr class="rule" />
                <div class="role role-user">{ "USER" }</div>
                <pre>{ &e.text }</pre>
            </div>
        },
        "thinking" => html! { <div class="role-thinking">{ markdown(&e.text) }</div> },
        "tool_use" => html! { <pre class="role-tool">{ format!("● {}", e.text) }</pre> },
        "tool_result" => html! { <pre class="dim">{ &e.text }</pre> },
        _ => html! { <div class="role-assistant">{ markdown(&e.text) }</div> },
    }
}

#[derive(Properties, PartialEq)]
struct ConversationProps {
    id: String,
}

#[function_component(Conversation)]
fn conversation(props: &ConversationProps) -> Html {
    let entries = use_state(Vec::<api::Entry>::new);
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
                    Err(e) => error.set(Some(e)),
                }

                let es = match EventSource::new(&format!("/api/sessions/{id}/events")) {
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
                                Err(e) => error.set(Some(e)),
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

    let on_send = {
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
                let req = Request::post(&format!("/api/sessions/{id}/messages"))
                    .json(&api::PostMessage { text })
                    .expect("serialize message");
                match req.send().await {
                    Ok(resp) if !resp.ok() => {
                        error.set(Some(format!("send failed: http {}", resp.status())))
                    }
                    Err(e) => error.set(Some(format!("send failed: {e}"))),
                    _ => {}
                }
            });
        })
    };

    let on_cancel = {
        let id = props.id.clone();
        Callback::from(move |_| {
            let id = id.clone();
            spawn_local(async move {
                let _ = Request::post(&format!("/api/sessions/{id}/cancel"))
                    .send()
                    .await;
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
                </div>
            </div>
            <div class="pane" ref={pane} onscroll={on_scroll}>
                <div class="column">
                { for entries.iter().map(render_entry) }
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
                    <textarea ref={input} rows="3" placeholder="message"></textarea>
                    <div class="actions">
                        <button onclick={on_send}>{ "send" }</button>
                        <button onclick={on_cancel}>{ "cancel turn" }</button>
                    </div>
                </div>
            </div>
        </div>
    }
}
