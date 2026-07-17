//! myco-gui: a Yew (wasm) web frontend for `myco --mode server`.
//!
//! Talks to `/api` (REST) and subscribes to a per-session Server-Sent Events
//! feed for live turns. Renders multiple sessions, streaming assistant text /
//! thinking, and an expandable tool/subagent tree — a richer window onto the
//! same runtime the CLI drives.

mod api;

use std::collections::HashMap;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, MessageEvent};
use yew::prelude::*;

use api::{HostView, MessageView, SessionDetail, SessionSummary, WireEvent};

const MODELS: &[&str] = &[
    "grok-4.5-build",
    "claude-haiku-4-5",
    "claude-sonnet-4-6",
    "claude-opus-4-8",
    "claude-fable-5",
];

/// In-flight streaming state for the currently open session's live turn.
#[derive(Clone, PartialEq, Default)]
struct LiveTurn {
    running: bool,
    /// Assistant text accumulated this turn (streamed).
    text: String,
    /// Thinking summary accumulated this turn (streamed).
    thinking: String,
    /// Running tools by id, in start order.
    tool_order: Vec<String>,
    tools: HashMap<String, LiveTool>,
    error: Option<String>,
}

#[derive(Clone, PartialEq)]
struct LiveTool {
    name: String,
    input: serde_json::Value,
    depth: usize,
    finished: bool,
    is_error: bool,
}

#[function_component(App)]
fn app() -> Html {
    let sessions = use_state(Vec::<SessionSummary>::new);
    let hosts = use_state(Vec::<HostView>::new);
    let selected = use_state(|| None::<String>);
    let detail = use_state(|| None::<SessionDetail>);
    let live = use_state(LiveTurn::default);
    let new_model = use_state(|| MODELS[0].to_string());
    let status = use_state(String::new);
    let version = use_state(String::new);

    // Initial load of the session list and hosts.
    {
        let sessions = sessions.clone();
        let hosts = hosts.clone();
        let version = version.clone();
        use_effect_with((), move |_| {
            reload_sessions(sessions.clone());
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(h) = api::hosts().await {
                    hosts.set(h);
                }
                if let Ok(hp) = api::health().await {
                    version.set(hp.version);
                }
            });
            || ()
        });
    }

    // When the selected session changes: fetch its transcript and (re)subscribe
    // to its SSE feed. The EventSource is torn down on change/unmount.
    {
        let selected = selected.clone();
        let detail = detail.clone();
        let live = live.clone();
        let sessions = sessions.clone();
        use_effect_with((*selected).clone(), move |sel| {
            let cleanup: Box<dyn FnOnce()> = match sel.clone() {
                None => Box::new(|| ()),
                Some(id) => {
                    // Load transcript snapshot.
                    {
                        let detail = detail.clone();
                        let live = live.clone();
                        let id = id.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            match api::get_session(&id).await {
                                Ok(d) => {
                                    live.set(LiveTurn {
                                        running: d.running,
                                        ..LiveTurn::default()
                                    });
                                    detail.set(Some(d));
                                }
                                Err(_) => detail.set(None),
                            }
                        });
                    }
                    subscribe(&id, live.clone(), detail.clone(), sessions.clone())
                }
            };
            move || cleanup()
        });
    }

    let on_select = {
        let selected = selected.clone();
        Callback::from(move |id: String| selected.set(Some(id)))
    };

    let on_new = {
        let new_model = new_model.clone();
        let sessions = sessions.clone();
        let selected = selected.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let model = (*new_model).clone();
            let sessions = sessions.clone();
            let selected = selected.clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match api::create_session(&model).await {
                    Ok(d) => {
                        selected.set(Some(d.id.clone()));
                        reload_sessions(sessions);
                    }
                    Err(e) => status.set(format!("create failed: {e}")),
                }
            });
        })
    };

    let on_model_change = {
        let new_model = new_model.clone();
        Callback::from(move |e: Event| {
            let sel: web_sys::HtmlSelectElement = e.target_unchecked_into();
            new_model.set(sel.value());
        })
    };

    let on_send = {
        let selected = selected.clone();
        let live = live.clone();
        let status = status.clone();
        Callback::from(move |text: String| {
            let Some(id) = (*selected).clone() else {
                return;
            };
            // Optimistically mark running and clear the previous turn's stream.
            live.set(LiveTurn {
                running: true,
                ..LiveTurn::default()
            });
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = api::send_message(&id, &text).await {
                    status.set(format!("send failed: {e}"));
                }
            });
        })
    };

    let on_cancel = {
        let selected = selected.clone();
        Callback::from(move |_| {
            let Some(id) = (*selected).clone() else {
                return;
            };
            wasm_bindgen_futures::spawn_local(async move {
                let _ = api::cancel(&id).await;
            });
        })
    };

    html! {
        <>
            <Sidebar
                sessions={(*sessions).clone()}
                selected={(*selected).clone()}
                new_model={(*new_model).clone()}
                version={(*version).clone()}
                on_select={on_select}
                on_new={on_new}
                on_model_change={on_model_change}
            />
            <Main
                detail={(*detail).clone()}
                hosts={(*hosts).clone()}
                live={(*live).clone()}
                status={(*status).clone()}
                on_send={on_send}
                on_cancel={on_cancel}
            />
        </>
    }
}

fn reload_sessions(sessions: UseStateHandle<Vec<SessionSummary>>) {
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(list) = api::list_sessions().await {
            sessions.set(list);
        }
    });
}

/// Open an [`EventSource`] on the session feed and fold events into `live`.
///
/// Returns a cleanup closure that closes the source. The handler `Closure` is
/// kept alive by moving it into the cleanup closure (dropped on teardown, after
/// the source is closed).
fn subscribe(
    id: &str,
    live: UseStateHandle<LiveTurn>,
    detail: UseStateHandle<Option<SessionDetail>>,
    sessions: UseStateHandle<Vec<SessionSummary>>,
) -> Box<dyn FnOnce()> {
    let es = match EventSource::new(&api::events_url(id)) {
        Ok(es) => es,
        Err(_) => return Box::new(|| ()),
    };

    let sess_id = id.to_string();
    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let Some(text) = ev.data().as_string() else {
            return;
        };
        let Ok(event) = serde_json::from_str::<WireEvent>(&text) else {
            return;
        };
        apply_event(event, &live, &detail, &sessions, &sess_id);
    });
    es.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

    Box::new(move || {
        es.close();
        drop(onmessage);
    })
}

/// Fold one live event into the streaming turn state; on turn end, refetch the
/// canonical transcript so persisted messages replace the ephemeral stream.
fn apply_event(
    event: WireEvent,
    live: &UseStateHandle<LiveTurn>,
    detail: &UseStateHandle<Option<SessionDetail>>,
    sessions: &UseStateHandle<Vec<SessionSummary>>,
    id: &str,
) {
    let mut cur = (**live).clone();
    match event {
        WireEvent::TextDelta { text } => {
            cur.running = true;
            cur.text.push_str(&text);
            live.set(cur);
        }
        WireEvent::ThinkingDelta { text } => {
            cur.running = true;
            cur.thinking.push_str(&text);
            live.set(cur);
        }
        WireEvent::ToolStarted {
            tool_use_id,
            name,
            input,
        } => {
            cur.running = true;
            if !cur.tools.contains_key(&tool_use_id) {
                cur.tool_order.push(tool_use_id.clone());
            }
            cur.tools.insert(
                tool_use_id,
                LiveTool {
                    name,
                    input,
                    depth: 0,
                    finished: false,
                    is_error: false,
                },
            );
            live.set(cur);
        }
        WireEvent::ToolFinished {
            tool_use_id,
            is_error,
        } => {
            if let Some(t) = cur.tools.get_mut(&tool_use_id) {
                t.finished = true;
                t.is_error = is_error;
            }
            live.set(cur);
        }
        WireEvent::TurnError { message } => {
            cur.running = false;
            cur.error = Some(message);
            live.set(cur);
        }
        WireEvent::TurnFinished {} | WireEvent::AgentFinished { .. } => {
            // Refetch the persisted transcript and clear the streaming buffer.
            let id = id.to_string();
            let detail = detail.clone();
            let live = live.clone();
            let sessions = sessions.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(d) = api::get_session(&id).await {
                    // Only clear the live buffer once the turn is truly over.
                    if !d.running {
                        live.set(LiveTurn::default());
                    }
                    detail.set(Some(d));
                }
                reload_sessions(sessions);
            });
        }
        WireEvent::Lagged { .. } | WireEvent::AgentStarted { .. } | WireEvent::Unknown => {}
    }
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

#[derive(Properties, PartialEq)]
struct SidebarProps {
    sessions: Vec<SessionSummary>,
    selected: Option<String>,
    new_model: String,
    version: String,
    on_select: Callback<String>,
    on_new: Callback<MouseEvent>,
    on_model_change: Callback<Event>,
}

#[function_component(Sidebar)]
fn sidebar(props: &SidebarProps) -> Html {
    let items = props.sessions.iter().map(|s| {
        let id = s.id.clone();
        let is_active = props.selected.as_deref() == Some(&s.id);
        let on_select = props.on_select.clone();
        let onclick = Callback::from(move |_| on_select.emit(id.clone()));
        let label = s
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| s.snippet.clone())
            .unwrap_or_else(|| format!("session {}", &s.id[..8.min(s.id.len())]));
        html! {
            <div class={classes!("session-item", is_active.then_some("active"))} {onclick}>
                <div class="title">
                    { label }
                    { if s.live { html!{ <span class="badge live">{"live"}</span> } } else { html!{} } }
                </div>
                <div class="meta">{ format!("{} · {} msgs", s.model, s.message_count) }</div>
            </div>
        }
    });

    let model_options = MODELS.iter().map(|m| {
        html! { <option value={*m} selected={props.new_model == *m}>{ *m }</option> }
    });

    html! {
        <div class="sidebar">
            <h1>{ "myco " }<span class="dim">{ format!("gui · v{}", props.version) }</span></h1>
            <div class="session-list">{ for items }</div>
            <div class="actions">
                <select onchange={props.on_model_change.clone()}>{ for model_options }</select>
                <button onclick={props.on_new.clone()}>{ "+ new session" }</button>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Main pane
// ---------------------------------------------------------------------------

#[derive(Properties, PartialEq)]
struct MainProps {
    detail: Option<SessionDetail>,
    hosts: Vec<HostView>,
    live: LiveTurn,
    status: String,
    on_send: Callback<String>,
    on_cancel: Callback<MouseEvent>,
}

#[function_component(Main)]
fn main_pane(props: &MainProps) -> Html {
    let Some(detail) = props.detail.clone() else {
        return html! {
            <div class="main">
                <HostBar hosts={props.hosts.clone()} />
                <div class="empty">{ "Select or create a session to begin." }</div>
            </div>
        };
    };

    let running = props.live.running || detail.running;

    html! {
        <div class="main">
            <div class="topbar">
                <span class="grow">
                    { detail.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| detail.id.clone()) }
                </span>
                <span class="host">{ detail.model.clone() }</span>
                { if running { html!{ <span class="host up">{"running"}</span> } } else { html!{} } }
            </div>
            <HostBar hosts={props.hosts.clone()} />
            <Scratchpad id={detail.id.clone()} value={detail.scratchpad.clone()} />
            <div class="transcript">
                { for detail.messages.iter().map(render_message) }
                { render_live(&props.live) }
                { if !props.status.is_empty() {
                    html!{ <div class="err">{ props.status.clone() }</div> }
                } else { html!{} } }
            </div>
            <Composer
                running={running}
                on_send={props.on_send.clone()}
                on_cancel={props.on_cancel.clone()}
            />
        </div>
    }
}

#[derive(Properties, PartialEq)]
struct HostBarProps {
    hosts: Vec<HostView>,
}
#[function_component(HostBar)]
fn host_bar(props: &HostBarProps) -> Html {
    if props.hosts.is_empty() {
        return html! {};
    }
    html! {
        <div class="topbar hosts">
            { for props.hosts.iter().map(|h| {
                let cls = if h.connected { "host up" } else if h.error.is_some() { "host down" } else { "host" };
                let label = if h.in_process { format!("{} (local)", h.name) } else { h.name.clone() };
                html!{ <span class={cls} title={h.error.clone().unwrap_or_default()}>{ label }</span> }
            }) }
        </div>
    }
}

fn render_message(m: &MessageView) -> Html {
    match m.role.as_str() {
        "tool" => html! {
            <div class="msg">
                { for m.tool_results.iter().map(|r| {
                    let icon = if r.is_error { html!{ <span class="fail">{"✗ "}</span> } } else { html!{ <span class="ok">{"✓ "}</span> } };
                    let body = r.content.iter().map(|c| c.text.clone()).collect::<Vec<_>>().join("\n");
                    html!{
                        <details class="tool">
                            <summary>{ icon }{ format!("tool result {}", short(&r.id)) }</summary>
                            <div class="body"><pre>{ body }</pre></div>
                        </details>
                    }
                }) }
            </div>
        },
        role => {
            let thinking: Html = m
                .content
                .iter()
                .filter(|c| c.kind == "thinking")
                .map(|c| html! { <div class="thinking">{ c.text.clone() }</div> })
                .collect();
            let answer: String = m
                .content
                .iter()
                .filter(|c| c.kind == "text")
                .map(|c| c.text.clone())
                .collect::<Vec<_>>()
                .join("\n");
            html! {
                <div class={classes!("msg", role.to_string())}>
                    <div class="role">{ role }</div>
                    { thinking }
                    { if !answer.is_empty() { html!{ <div class="bubble">{ answer }</div> } } else { html!{} } }
                    { for m.tool_uses.iter().map(|t| html!{
                        <details class="tool">
                            <summary><span class="ok">{"→ "}</span>{ format!("{} {}", t.name, short(&t.id)) }</summary>
                            <div class="body"><pre>{ pretty(&t.input) }</pre></div>
                        </details>
                    }) }
                </div>
            }
        }
    }
}

/// Render the ephemeral, in-flight turn (streaming text + running tools).
fn render_live(live: &LiveTurn) -> Html {
    if !live.running && live.text.is_empty() && live.thinking.is_empty() && live.error.is_none() {
        return html! {};
    }
    let tools = live.tool_order.iter().filter_map(|id| {
        live.tools.get(id).map(|t| {
            let icon = if !t.finished {
                html! { <span class="spin">{"● "}</span> }
            } else if t.is_error {
                html! { <span class="fail">{"✗ "}</span> }
            } else {
                html! { <span class="ok">{"✓ "}</span> }
            };
            let depth_cls = (t.depth > 0).then_some("depth");
            html! {
                <details class={classes!("tool", depth_cls)} open={!t.finished}>
                    <summary>{ icon }{ format!("{} {}", t.name, short(id)) }</summary>
                    <div class="body"><pre>{ pretty(&t.input) }</pre></div>
                </details>
            }
        })
    });
    html! {
        <div class="msg assistant">
            <div class="role">{ if live.running { "assistant · streaming" } else { "assistant" } }</div>
            { if !live.thinking.is_empty() { html!{ <div class="thinking">{ live.thinking.clone() }</div> } } else { html!{} } }
            { if !live.text.is_empty() { html!{ <div class="bubble">{ live.text.clone() }</div> } } else { html!{} } }
            { for tools }
            { if let Some(e) = &live.error { html!{ <div class="err">{ e.clone() }</div> } } else { html!{} } }
        </div>
    }
}

// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------

#[derive(Properties, PartialEq)]
struct ComposerProps {
    running: bool,
    on_send: Callback<String>,
    on_cancel: Callback<MouseEvent>,
}

#[function_component(Composer)]
fn composer(props: &ComposerProps) -> Html {
    let text = use_state(String::new);

    let oninput = {
        let text = text.clone();
        Callback::from(move |e: InputEvent| {
            let ta: web_sys::HtmlTextAreaElement = e.target_unchecked_into();
            text.set(ta.value());
        })
    };

    let do_send = {
        let text = text.clone();
        let on_send = props.on_send.clone();
        Callback::from(move |_| {
            let value = (*text).clone();
            if value.trim().is_empty() {
                return;
            }
            on_send.emit(value);
            text.set(String::new());
        })
    };

    // Enter submits; Shift-Enter inserts a newline.
    let onkeydown = {
        let do_send = do_send.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" && !e.shift_key() {
                e.prevent_default();
                do_send.emit(());
            }
        })
    };

    let onclick_send = {
        let do_send = do_send.clone();
        Callback::from(move |_: MouseEvent| do_send.emit(()))
    };

    html! {
        <div class="composer">
            <textarea
                placeholder="Message myco…  (Enter to send, Shift-Enter for newline)"
                value={(*text).clone()}
                {oninput}
                {onkeydown}
            />
            <div class="col">
                <button onclick={onclick_send} disabled={props.running}>{ "send" }</button>
                <button onclick={props.on_cancel.clone()} disabled={!props.running}>{ "cancel" }</button>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Scratchpad (session metadata edit; PATCH on blur)
// ---------------------------------------------------------------------------

#[derive(Properties, PartialEq)]
struct ScratchpadProps {
    id: String,
    value: String,
}

#[function_component(Scratchpad)]
fn scratchpad(props: &ScratchpadProps) -> Html {
    let id = props.id.clone();
    // Persist on blur so we do not PATCH on every keystroke.
    let onblur = Callback::from(move |e: FocusEvent| {
        let ta: web_sys::HtmlTextAreaElement = e.target_unchecked_into();
        let value = ta.value();
        let id = id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = api::set_scratchpad(&id, &value).await;
        });
    });
    html! {
        <div class="scratchpad">
            <textarea
                placeholder="scratchpad (notes; saved on blur)"
                value={props.value.clone()}
                {onblur}
            />
        </div>
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn short(id: &str) -> String {
    let n = id.len().min(8);
    id[id.len() - n..].to_string()
}

fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn main() {
    let root = document()
        .get_element_by_id("app-root")
        .expect("#app-root element");
    yew::Renderer::<App>::with_root(root).render();
}

/// Minimal `document()` accessor without pulling in gloo-utils.
fn document() -> web_sys::Document {
    web_sys::window()
        .expect("window")
        .document()
        .expect("document")
}
