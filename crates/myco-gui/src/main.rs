//! myco-gui Phase 1: CLI-parity transcript UI.
//!
//! A slightly more polished CLI in the browser:
//! - full-width `<hr>` section rules (not 72-col box drawing)
//! - color for USER / ASSISTANT / ERROR / thinking / tools
//! - sections mirror the CLI: USER (double rule), ASSISTANT (thin rule), ERROR;
//!   `Thinking: …` paragraphs; `name(<pretty json>)` tool paras; `USER used/max`
//!
//! No multi-session sidebar, host dashboard, scratchpad editor, or autocomplete.

mod api;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, KeyboardEvent, MessageEvent};
use yew::prelude::*;

use api::{MessageView, SessionDetail, WireEvent};

/// Pretty-print tool input like CLI `format_tool_invocation`.
fn format_tool_invocation(name: &str, input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            let pretty = serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
            format!("{name}({pretty})")
        }
        other => format!("{name}({other})"),
    }
}

#[derive(Clone, PartialEq, Default)]
struct LiveTurn {
    running: bool,
    text: String,
    thinking: String,
    /// Tool paragraphs accumulated this turn, in order.
    tools: Vec<String>,
    error: Option<String>,
    /// Last known context used (from Usage events).
    used_tokens: Option<u64>,
}

#[function_component(App)]
fn app() -> Html {
    let session = use_state(|| None::<SessionDetail>);
    let live = use_state(LiveTurn::default);
    let draft = use_state(String::new);
    let status = use_state(String::new);
    let version = use_state(String::new);

    // Bootstrap: create a fresh session (CLI-like single transcript).
    {
        let session = session.clone();
        let live = live.clone();
        let status = status.clone();
        let version = version.clone();
        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                match api::health().await {
                    Ok(h) => version.set(h.version),
                    Err(e) => status.set(format!("health: {e}")),
                }
                match api::create_session().await {
                    Ok(d) => {
                        live.set(LiveTurn {
                            running: d.running,
                            ..LiveTurn::default()
                        });
                        session.set(Some(d));
                    }
                    Err(e) => status.set(format!("create session: {e}")),
                }
            });
            || ()
        });
    }

    // SSE subscription for the open session.
    {
        let session = session.clone();
        let live = live.clone();
        let status = status.clone();
        use_effect_with((*session).clone().map(|s| s.id), move |id| {
            let cleanup: Box<dyn FnOnce()> = match id.clone() {
                None => Box::new(|| ()),
                Some(id) => subscribe(id, live.clone(), session.clone(), status.clone()),
            };
            move || cleanup()
        });
    }

    let on_input = {
        let draft = draft.clone();
        Callback::from(move |e: InputEvent| {
            let ta: web_sys::HtmlTextAreaElement = e.target_unchecked_into();
            draft.set(ta.value());
        })
    };

    let send = {
        let draft = draft.clone();
        let session = session.clone();
        let live = live.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let text = (*draft).clone();
            if text.trim().is_empty() {
                return;
            }
            let Some(s) = (*session).clone() else { return };
            if live.running {
                return;
            }
            draft.set(String::new());
            // Optimistic USER section: append a synthetic user message locally.
            let mut s2 = s.clone();
            s2.messages.push(MessageView {
                role: "user".into(),
                content: vec![api::WireContent {
                    kind: "text".into(),
                    text: text.clone(),
                }],
                tool_uses: Vec::new(),
            });
            session.set(Some(s2));
            live.set(LiveTurn {
                running: true,
                ..LiveTurn::default()
            });
            status.set(String::new());
            let session = session.clone();
            let live = live.clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = api::send_message(&s.id, &text).await {
                    live.set(LiveTurn {
                        running: false,
                        error: Some(e.clone()),
                        ..LiveTurn::default()
                    });
                    status.set(e);
                    // Reload transcript to drop the optimistic user msg if rejected.
                    if let Ok(d) = api::get_session(&s.id).await {
                        session.set(Some(d));
                    }
                }
            });
        })
    };

    let on_keydown = {
        let send = send.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" && !e.shift_key() {
                e.prevent_default();
                send.emit(());
            }
        })
    };

    let on_cancel = {
        let session = session.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let Some(s) = (*session).clone() else { return };
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = api::cancel(&s.id).await {
                    status.set(e);
                }
            });
        })
    };

    let context_max = session
        .as_ref()
        .map(|s| s.context_window_tokens)
        .unwrap_or(0);
    let used = live.used_tokens.unwrap_or(0);

    html! {
        <>
            <div class="top">
                <span class="title">{ "myco" }</span>
                <span>{ session.as_ref().map(|s| s.model.clone()).unwrap_or_default() }</span>
                if !version.is_empty() {
                    <span>{ format!("v{}", *version) }</span>
                }
                <span class="grow"></span>
                if let Some(s) = session.as_ref() {
                    <span title={s.id.clone()}>{ format_id_short(&s.id) }</span>
                }
            </div>

            <div class="transcript" id="transcript">
                if let Some(s) = session.as_ref() {
                    { for s.messages.iter().map(render_message) }
                } else if status.is_empty() {
                    <div class="empty">{ "starting session…" }</div>
                }

                // Live ASSISTANT / ERROR for the in-flight turn.
                if live.running || !live.text.is_empty() || !live.thinking.is_empty()
                    || !live.tools.is_empty() || live.error.is_some()
                {
                    if let Some(err) = &live.error {
                        <hr class="rule error" />
                        <div class="header error">{ "ERROR" }</div>
                        <div class="para">{ err }</div>
                    } else {
                        <hr class="rule assistant" />
                        <div class="header assistant">{ "ASSISTANT" }</div>
                        if !live.thinking.is_empty() {
                            <div class="para thinking">{
                                { format!("Thinking: {}", live.thinking) }
                            }</div>
                        }
                        { for live.tools.iter().map(|t| html! {
                            <div class="para tool">{ t }</div>
                        }) }
                        if !live.text.is_empty() {
                            <div class="para">{ live.text.clone() }</div>
                        } else if live.running && live.thinking.is_empty() && live.tools.is_empty() {
                            <div class="para thinking">{ "…" }</div>
                        }
                    }
                }
            </div>

            <div class={classes!("status", (!status.is_empty()).then_some("err"))}>
                { (*status).clone() }
            </div>
            <div class="composer">
                <textarea
                    value={(*draft).clone()}
                    oninput={on_input}
                    onkeydown={on_keydown}
                    placeholder={"Message (Enter to send, Shift-Enter for newline)"}
                    disabled={session.is_none() || live.running}
                />
                <div>
                    if live.running {
                        <button onclick={on_cancel}>{ "Cancel" }</button>
                    } else {
                        <button onclick={move |_| send.emit(())} disabled={session.is_none()}>
                            { "Send" }
                        </button>
                    }
                    if context_max > 0 {
                        <div class="status" style="margin-top:6px;text-align:right;">
                            { format!("USER {used}/{context_max}") }
                        </div>
                    }
                </div>
            </div>
        </>
    }
}

fn format_id_short(id: &str) -> String {
    if id.len() > 8 {
        format!("{}…", &id[..8])
    } else {
        id.to_string()
    }
}

fn render_message(m: &MessageView) -> Html {
    match m.role.as_str() {
        "user" => {
            let text = m
                .content
                .iter()
                .filter(|c| c.kind == "text")
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("");
            html! {
                <>
                    <hr class="rule user" />
                    <div class="header user">{ "USER" }</div>
                    <div class="para">{ text }</div>
                </>
            }
        }
        "assistant" => {
            let mut parts: Vec<Html> = Vec::new();
            for c in &m.content {
                match c.kind.as_str() {
                    "thinking" => {
                        parts.push(html! {
                            <div class="para thinking">{
                                { format!("Thinking: {}", c.text) }
                            }</div>
                        });
                    }
                    "text" => {
                        if !c.text.is_empty() {
                            parts.push(html! { <div class="para">{ c.text.clone() }</div> });
                        }
                    }
                    _ => {}
                }
            }
            for t in &m.tool_uses {
                let inv = format_tool_invocation(&t.name, &t.input);
                parts.push(html! { <div class="para tool">{ inv }</div> });
            }
            html! {
                <>
                    <hr class="rule assistant" />
                    <div class="header assistant">{ "ASSISTANT" }</div>
                    { for parts }
                </>
            }
        }
        // Tool result messages are not shown as headed sections in the CLI.
        _ => html! {},
    }
}

fn finish_turn(
    err: Option<String>,
    live: &UseStateHandle<LiveTurn>,
    session: &UseStateHandle<Option<SessionDetail>>,
    id: &str,
) {
    let session = session.clone();
    let live = live.clone();
    let used = live.used_tokens;
    let id = id.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        match api::get_session(&id).await {
            Ok(d) => {
                live.set(LiveTurn {
                    running: false,
                    used_tokens: used,
                    error: err,
                    ..LiveTurn::default()
                });
                session.set(Some(d));
            }
            Err(e) => {
                live.set(LiveTurn {
                    running: false,
                    error: Some(e),
                    used_tokens: used,
                    ..LiveTurn::default()
                });
            }
        }
    });
}

/// Subscribe to SSE; returns a cleanup closure that closes the EventSource.
fn subscribe(
    id: String,
    live: UseStateHandle<LiveTurn>,
    session: UseStateHandle<Option<SessionDetail>>,
    status: UseStateHandle<String>,
) -> Box<dyn FnOnce()> {
    let url = api::events_url(&id);
    let es = match EventSource::new(&url) {
        Ok(es) => es,
        Err(e) => {
            status.set(format!("sse open: {e:?}"));
            return Box::new(|| ());
        }
    };

    let live_c = live.clone();
    let session_c = session.clone();
    let status_c = status.clone();
    let id_c = id.clone();

    let on_message = Closure::wrap(Box::new(move |e: MessageEvent| {
        let Some(data) = e.data().as_string() else {
            return;
        };
        let Ok(ev) = serde_json::from_str::<WireEvent>(&data) else {
            return;
        };
        match ev {
            WireEvent::ThinkingDelta { text } => {
                let mut t = (*live_c).clone();
                t.running = true;
                t.thinking.push_str(&text);
                live_c.set(t);
            }
            WireEvent::TextDelta { text } => {
                let mut t = (*live_c).clone();
                t.running = true;
                t.text.push_str(&text);
                live_c.set(t);
            }
            WireEvent::ToolStarted { name, input, .. } => {
                let mut t = (*live_c).clone();
                t.running = true;
                t.tools.push(format_tool_invocation(&name, &input));
                live_c.set(t);
            }
            WireEvent::Usage { usage } => {
                let mut t = (*live_c).clone();
                t.used_tokens = Some(usage.context_tokens());
                live_c.set(t);
            }
            WireEvent::TurnFinished { .. } => {
                finish_turn(None, &live_c, &session_c, &id_c);
            }
            WireEvent::TurnError { message } => {
                finish_turn(Some(message), &live_c, &session_c, &id_c);
            }
            WireEvent::Lagged { skipped } => {
                status_c.set(format!("sse lagged; skipped {skipped} events"));
            }
            _ => {}
        }
    }) as Box<dyn FnMut(_)>);

    es.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    // Keep the closure alive for the EventSource lifetime.
    on_message.forget();

    let es_close = es.clone();
    Box::new(move || {
        es_close.close();
    })
}

fn main() {
    // Prefer the explicit shell node so body chrome (if any) stays outside the app.
    let document = web_sys::window()
        .expect("window")
        .document()
        .expect("document");
    let root = document.get_element_by_id("app-root").expect("#app-root");
    yew::Renderer::<App>::with_root(root).render();
}
