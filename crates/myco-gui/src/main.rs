//! Minimal Yew client for the myco API server. Deliberately unstyled and
//! small: a session browser at `/` and one conversation per URL at
//! `/session/<id>`. Live output arrives over SSE; a slow poll reconciles the
//! transcript as a fallback.

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

async fn fetch_poll(id: &str) -> Option<api::Poll> {
    let url = format!("/api/sessions/{id}/poll?since=0");
    Request::get(&url).send().await.ok()?.json().await.ok()
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
        <div>
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
    }
}

// ---------------------------------------------------------------------------
// Conversation (one URL per session)
// ---------------------------------------------------------------------------

#[derive(Properties, PartialEq)]
struct ConversationProps {
    id: String,
}

#[function_component(Conversation)]
fn conversation(props: &ConversationProps) -> Html {
    let entries = use_state(Vec::<api::Entry>::new);
    let busy = use_state(|| false);
    // Text streamed since the last transcript refresh (SSE deltas).
    let streaming = use_state(String::new);
    let input = use_node_ref();

    // SSE: live deltas; TurnStarted/TurnFinished trigger a transcript refresh.
    {
        let entries = entries.clone();
        let busy = busy.clone();
        let streaming = streaming.clone();
        let id = props.id.clone();
        use_effect_with(id, move |id| {
            let id = id.clone();
            let alive = std::rc::Rc::new(std::cell::Cell::new(true));
            let alive2 = alive.clone();
            spawn_local(async move {
                // Initial transcript.
                if let Some(p) = fetch_poll(&id).await {
                    entries.set(p.entries);
                    busy.set(p.busy);
                }
                if let Ok(mut es) = EventSource::new(&format!("/api/sessions/{id}/events"))
                    && let Ok(mut stream) = es.subscribe("message")
                {
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
                        match ev {
                            api::StreamEvent::TurnStarted => {
                                busy.set(true);
                                streaming.set(String::new());
                                if let Some(p) = fetch_poll(&id).await {
                                    entries.set(p.entries);
                                }
                            }
                            api::StreamEvent::TextDelta { text } => {
                                streaming.set(format!("{}{}", *streaming, text));
                            }
                            api::StreamEvent::ThinkingDelta { text } => {
                                streaming.set(format!("{}{}", *streaming, text));
                            }
                            api::StreamEvent::ToolStarted { name, .. } => {
                                streaming.set(format!("{}\n[tool: {name}]\n", *streaming));
                            }
                            api::StreamEvent::TurnFinished => {
                                streaming.set(String::new());
                                if let Some(p) = fetch_poll(&id).await {
                                    entries.set(p.entries);
                                    busy.set(p.busy);
                                }
                            }
                        }
                    }
                    // Dropping `es` closes the connection.
                }
            });
            move || alive.set(false)
        });
    }

    // Slow reconciliation poll: covers SSE hiccups and other writers.
    {
        let entries = entries.clone();
        let busy = busy.clone();
        let id = props.id.clone();
        use_effect_with(id, move |id| {
            let id = id.clone();
            let alive = std::rc::Rc::new(std::cell::Cell::new(true));
            let alive2 = alive.clone();
            spawn_local(async move {
                while alive2.get() {
                    gloo_timers::future::TimeoutFuture::new(5_000).await;
                    if let Some(p) = fetch_poll(&id).await {
                        entries.set(p.entries);
                        busy.set(p.busy);
                    }
                }
            });
            move || alive.set(false)
        });
    }

    let on_send = {
        let id = props.id.clone();
        let input = input.clone();
        let busy = busy.clone();
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
            spawn_local(async move {
                let req = Request::post(&format!("/api/sessions/{id}/messages"))
                    .json(&api::PostMessage { text })
                    .unwrap();
                let _ = req.send().await;
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
        <div>
            <p><Link<Route> to={Route::Browser}>{ "← sessions" }</Link<Route>>
               { format!("  {}  ", props.id) }
               { if *busy { html!{ <em>{ "(agent working…)" }</em> } } else { html!{} } }
            </p>
            <div>
                { for entries.iter().map(|e| html! {
                    <div>
                        <b>{ format!("{}: ", e.role) }</b>
                        <pre style="white-space: pre-wrap; display: inline;">{ &e.text }</pre>
                    </div>
                }) }
                { if !streaming.is_empty() { html! {
                    <div>
                        <b>{ "assistant (streaming): " }</b>
                        <pre style="white-space: pre-wrap; display: inline;">{ &*streaming }</pre>
                    </div>
                } } else { html!{} } }
            </div>
            <textarea ref={input} rows="4" cols="100" placeholder="message (Enter does not send)"></textarea>
            <br />
            <button onclick={on_send}>{ "send" }</button>
            <button onclick={on_cancel}>{ "cancel turn" }</button>
        </div>
    }
}
