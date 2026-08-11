//! The wasm edge: dispatch, render, effects. An `onClick` is a dispatch;
//! an effect's completion is a dispatch; nothing here holds state or
//! contains logic — the reducer is the whole brain.

use std::cell::RefCell;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::core::{
    Action, Effect, Session, Stage, State, palette_rows, reduce, reserved_chord, wants_key,
};

const TOKEN_KEY: &str = "myco.token";

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    /// The live event socket — edge-owned connection state (the reducer
    /// owns *what* is watched; the edge owns the wire that carries it).
    static SOCKET: RefCell<Option<web_sys::WebSocket>> = const { RefCell::new(None) };
}

/// Send one op frame on the live socket, if any. A closed socket is fine:
/// FeedOpened re-arms every watch when it comes back.
fn send_op(op: &str, id: &str) {
    SOCKET.with(|socket| {
        if let Some(socket) = socket.borrow().as_ref() {
            let frame = serde_json::json!({ "op": op, "id": id }).to_string();
            let _ = socket.send_with_str(&frame);
        }
    });
}

#[wasm_bindgen(start)]
pub fn start() {
    attach_keyboard();
    dispatch(Action::Boot {
        token: storage().and_then(|s| s.get_item(TOKEN_KEY).ok().flatten()),
    });
}

/// One global keydown listener, attached once. preventDefault is
/// synchronous, so the edge asks the pure [`wants_key`] policy before
/// committing the keystroke to the reducer — the one place edge and core
/// must agree in the same tick.
fn attach_keyboard() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let on_key =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |event: web_sys::KeyboardEvent| {
            let (key, ctrl, alt, meta) = (
                event.key(),
                event.ctrl_key(),
                event.alt_key(),
                event.meta_key(),
            );
            if reserved_chord(&key, ctrl, meta) {
                event.prevent_default();
                dispatch(Action::PaletteToggled);
                return;
            }
            let renaming = STATE.with(|s| s.borrow().renaming.is_some());
            if renaming {
                if key == "Escape" {
                    event.prevent_default();
                    dispatch(Action::RenameDismissed);
                }
                return;
            }
            let palette_open = STATE.with(|s| s.borrow().palette.is_some());
            if palette_open {
                // The palette owns the keyboard: navigation here, typing
                // in its own input (ordinary DOM), the rest stays put.
                match key.as_str() {
                    "Escape" => {
                        event.prevent_default();
                        dispatch(Action::PaletteDismissed);
                    }
                    "ArrowDown" => {
                        event.prevent_default();
                        dispatch(Action::PaletteMoved { delta: 1 });
                    }
                    "ArrowUp" => {
                        event.prevent_default();
                        dispatch(Action::PaletteMoved { delta: -1 });
                    }
                    _ => {}
                }
                return;
            }
            let wanted = STATE.with(|s| wants_key(&s.borrow(), &key, ctrl, meta));
            if wanted {
                event.prevent_default();
                dispatch(Action::KeyPressed { key, ctrl, alt });
            }
        });
    let _ = document.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());
    on_key.forget();
}

/// The one entry point for every action from every source. Reduce, render,
/// then run the effects — whose completions re-enter here.
fn dispatch(action: Action) {
    let effects = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let effects = reduce(&mut state, action);
        render(&state);
        effects
    });
    for effect in effects {
        run(effect);
    }
    // After the borrow is gone: a focused tty may need a resize. Measuring
    // (and dispatching) from inside render re-entered STATE and panicked.
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let focused_tty = STATE.with(|s| {
        let state = s.borrow();
        state.workspace.selected.clone().filter(|_| {
            state.workspace.panes.iter().any(|p| {
                Some(p.id.as_str()) == state.workspace.selected.as_deref() && p.kind == "tty"
            })
        })
    });
    measure_focused_tty(&document, focused_tty);
}

fn run(effect: Effect) {
    match effect {
        Effect::Whoami { token } => wasm_bindgen_futures::spawn_local(async move {
            dispatch(
                match fetch("GET", "/api/whoami", token.as_deref(), None).await {
                    Ok((status, body)) => Action::WhoamiAnswered { status, body },
                    Err(what) => Action::NetworkFailed { what },
                },
            );
        }),
        Effect::RedeemCode { username, code } => wasm_bindgen_futures::spawn_local(async move {
            let form = format!(
                "grant_type=code&username={}&code={}",
                url_encode(&username),
                url_encode(&code)
            );
            dispatch(
                match fetch("POST", "/api/auth/token", None, Some((FORM, form))).await {
                    Ok((status, body)) => Action::TokenAnswered { status, body },
                    Err(what) => Action::NetworkFailed { what },
                },
            );
        }),
        Effect::PersistToken(token) => {
            if let Some(storage) = storage() {
                let _ = storage.set_item(TOKEN_KEY, &token);
            }
        }
        Effect::ClearToken => {
            if let Some(storage) = storage() {
                let _ = storage.remove_item(TOKEN_KEY);
            }
        }
        Effect::Logout { token } => wasm_bindgen_futures::spawn_local(async move {
            // Best-effort revocation; the local state is already out.
            let _ = fetch("POST", "/api/auth/logout", Some(&token), None).await;
        }),
        Effect::EnrollPasskey { token } => wasm_bindgen_futures::spawn_local(async move {
            dispatch(enroll_passkey(&token).await);
        }),
        Effect::PasskeySignIn { username } => wasm_bindgen_futures::spawn_local(async move {
            dispatch(passkey_sign_in(&username).await);
        }),
        Effect::FetchKinds { token } => wasm_bindgen_futures::spawn_local(async move {
            dispatch(match fetch("GET", "/api/kinds", Some(&token), None).await {
                Ok((status, body)) => Action::KindsAnswered { status, body },
                Err(what) => Action::NetworkFailed { what },
            });
        }),
        Effect::FetchInstances { token } => wasm_bindgen_futures::spawn_local(async move {
            dispatch(
                match fetch("GET", "/api/instances", Some(&token), None).await {
                    Ok((status, body)) => Action::InstancesAnswered { status, body },
                    Err(what) => Action::NetworkFailed { what },
                },
            );
        }),
        Effect::OpenFeed { token } => open_feed(token),
        Effect::CreateInstance {
            token,
            kind,
            project,
            title,
        } => wasm_bindgen_futures::spawn_local(async move {
            let body =
                serde_json::json!({ "kind": kind, "project": project, "title": title }).to_string();
            dispatch(
                match fetch("POST", "/api/instances", Some(&token), Some((JSON, body))).await {
                    Ok((status, body)) => Action::CreateAnswered { status, body },
                    Err(what) => Action::NetworkFailed { what },
                },
            );
        }),
        Effect::Watch { id } => send_op("watch", &id),
        Effect::Unwatch { id } => send_op("unwatch", &id),
        Effect::CallVerb {
            origin,
            token,
            id,
            verb,
            args,
        } => wasm_bindgen_futures::spawn_local(async move {
            let url = format!("/api/instances/{id}/verbs/{verb}");
            let body = (!args.is_empty()).then_some((JSON, args));
            dispatch(match fetch("POST", &url, Some(&token), body).await {
                Ok((status, body)) => Action::VerbReplied {
                    origin,
                    id,
                    verb,
                    status,
                    body,
                },
                Err(what) => Action::NetworkFailed { what },
            });
        }),
    }
}

/// The event socket. Browsers cannot set WS headers, so the token rides
/// `?token=` — the M1 concession, checked like the header. Every message
/// is only a staleness hint (the reducer re-lists); a close dispatches
/// FeedDropped and retries after a beat, forever, until sign-out drops
/// the token and the reopened socket is refused.
fn open_feed(token: String) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let proto = if window.location().protocol().ok().as_deref() == Some("https:") {
        "wss"
    } else {
        "ws"
    };
    let host = window.location().host().unwrap_or_default();
    let url = format!("{proto}://{host}/api/ws?token={}", url_encode(&token));
    let Ok(socket) = web_sys::WebSocket::new(&url) else {
        dispatch(Action::FeedDropped);
        return;
    };

    let on_open = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        dispatch(Action::FeedOpened);
    });
    socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();

    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            let Some(text) = event.data().as_string() else {
                return;
            };
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
                return;
            };
            let id = || frame["id"].as_str().unwrap_or_default().to_string();
            dispatch(match frame["t"].as_str() {
                Some("mark") => Action::Marked { id: id() },
                Some("gone") => Action::InstanceGone { id: id() },
                // Events and lag both mean "the listing may be stale".
                _ => Action::FeedEvent,
            });
        });
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    let retry_token = token.clone();
    let on_close = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        dispatch(Action::FeedDropped);
        let token = retry_token.clone();
        let retry = Closure::once_into_js(move || {
            // Only retry while this token is still the live one.
            let still = STATE.with(|s| s.borrow().token.as_deref() == Some(token.as_str()));
            if still {
                run(Effect::OpenFeed { token });
            }
        });
        if let Some(window) = web_sys::window() {
            let _ = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(retry.unchecked_ref(), 2000);
        }
    });
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();

    SOCKET.with(|slot| *slot.borrow_mut() = Some(socket));
}

// ---------------------------------------------------------------------------
// WebAuthn ceremonies
//
// webauthn-rs speaks the exact JSON `PublicKeyCredential.parse*FromJSON`
// and `credential.toJSON()` speak, so the ceremony is three passes through
// a thin JS shim (the byte-mangling the older API required is exactly what
// these Level-3 functions exist to delete).
// ---------------------------------------------------------------------------

async fn enroll_passkey(token: &str) -> Action {
    let (status, challenge) = match fetch(
        "POST",
        "/api/auth/passkey/register/start",
        Some(token),
        None,
    )
    .await
    {
        Ok(answer) => answer,
        Err(what) => return Action::NetworkFailed { what },
    };
    if status != 200 {
        return Action::PasskeyEnrollAnswered {
            status,
            body: challenge,
        };
    }
    let credential = match ceremony("create", "parseCreationOptionsFromJSON", &challenge).await {
        Ok(c) => c,
        Err(why) => return Action::PasskeyFailed { why },
    };
    match fetch(
        "POST",
        "/api/auth/passkey/register/finish",
        Some(token),
        Some((JSON, credential)),
    )
    .await
    {
        Ok((status, body)) => Action::PasskeyEnrollAnswered { status, body },
        Err(what) => Action::NetworkFailed { what },
    }
}

async fn passkey_sign_in(username: &str) -> Action {
    let start = serde_json::json!({ "username": username }).to_string();
    let (status, body) = match fetch(
        "POST",
        "/api/auth/passkey/login/start",
        None,
        Some((JSON, start)),
    )
    .await
    {
        Ok(answer) => answer,
        Err(what) => return Action::NetworkFailed { what },
    };
    if status != 200 {
        // The uniform non-enumeration answer lands on the form verbatim.
        return Action::TokenAnswered { status, body };
    }
    #[derive(serde::Deserialize)]
    struct Challenge {
        ticket: String,
        options: serde_json::Value,
    }
    let Ok(challenge) = serde_json::from_str::<Challenge>(&body) else {
        return Action::PasskeyFailed {
            why: "the server's challenge made no sense".into(),
        };
    };
    let assertion = match ceremony(
        "get",
        "parseRequestOptionsFromJSON",
        &challenge.options.to_string(),
    )
    .await
    {
        Ok(a) => a,
        Err(why) => return Action::PasskeyFailed { why },
    };
    let finish = format!(
        r#"{{"ticket":{},"credential":{}}}"#,
        serde_json::json!(challenge.ticket),
        assertion
    );
    match fetch(
        "POST",
        "/api/auth/passkey/login/finish",
        None,
        Some((JSON, finish)),
    )
    .await
    {
        Ok((status, body)) => Action::TokenAnswered { status, body },
        Err(what) => Action::NetworkFailed { what },
    }
}

/// One browser ceremony: options JSON in, credential JSON out. `method` is
/// `create` or `get`; `parser` the matching Level-3 parse function.
async fn ceremony(method: &str, parser: &str, options_json: &str) -> Result<String, String> {
    let body = format!(
        "return navigator.credentials.{method}({{ publicKey: \
         PublicKeyCredential.{parser}(JSON.parse(json).publicKey ?? JSON.parse(json)) }})\
         .then(c => JSON.stringify(c.toJSON()));"
    );
    let shim = js_sys::Function::new_with_args("json", &body);
    let promise = shim
        .call1(&JsValue::NULL, &JsValue::from_str(options_json))
        .map_err(|_| "this browser cannot run the passkey ceremony".to_string())?;
    let promise: js_sys::Promise = promise
        .dyn_into()
        .map_err(|_| "this browser cannot run the passkey ceremony".to_string())?;
    match wasm_bindgen_futures::JsFuture::from(promise).await {
        Ok(value) => value
            .as_string()
            .ok_or_else(|| "the ceremony returned nothing".into()),
        Err(_) => Err("the passkey prompt was dismissed".into()),
    }
}

const JSON: &str = "application/json";

const FORM: &str = "application/x-www-form-urlencoded";

async fn fetch(
    method: &str,
    url: &str,
    bearer: Option<&str>,
    body: Option<(&str, String)>,
) -> Result<(u16, String), String> {
    let window = web_sys::window().ok_or("no window")?;
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    let headers = web_sys::Headers::new().map_err(|_| "headers")?;
    if let Some(token) = bearer {
        let _ = headers.set("authorization", &format!("Bearer {token}"));
    }
    if let Some((content_type, payload)) = body {
        let _ = headers.set("content-type", content_type);
        init.set_body(&JsValue::from_str(&payload));
    }
    init.set_headers(&headers);
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str_and_init(url, &init))
        .await
        .map_err(|_| format!("{method} {url}"))?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "not a response")?;
    let status = response.status();
    let body = match response.text() {
        Ok(promise) => wasm_bindgen_futures::JsFuture::from(promise)
            .await
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    Ok((status, body))
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn url_encode(text: &str) -> String {
    js_sys::encode_uri_component(text)
        .as_string()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// Render is a pure function of state. At this stack position the whole
/// UI is one island; the workspace arrives with the tree and panes.
fn render(state: &State) {
    let html = match &state.session {
        Session::Checking => shell_card("", "reaching the server…", ""),
        Session::Unreachable { why } => shell_card("attn", &escape(why), ""),
        Session::SignedOut => sign_in_card(state),
        Session::SignedIn(user) => workspace_view(state, user),
    };
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    if let Some(body) = document.body() {
        body.set_inner_html(&html);
    }
    wire(&document);
}

fn shell_card(dot: &str, line: &str, extra: &str) -> String {
    format!(
        r#"<div class="shell-center">
             <div class="island shell-card">
               <div class="shell-brand"><span class="spore">●</span> myco</div>
               <div class="dim">a workspace of instances, shared by humans and agents</div>
               <div class="status-line"><span class="status-dot {dot}"></span><span>{line}</span></div>
               {extra}
             </div>
           </div>"#
    )
}

fn sign_in_card(state: &State) -> String {
    let error = match &state.sign_in.error {
        Some(why) => format!(r#"<div class="form-error">{}</div>"#, escape(why)),
        None => String::new(),
    };
    let busy = if state.sign_in.busy { "disabled" } else { "" };
    format!(
        r#"<div class="shell-center">
             <div class="island shell-card">
               <div class="shell-brand"><span class="spore">●</span> myco</div>
               <div class="dim">sign in with your one-time code — minted by the operator,
                 or printed where the server started</div>
               <form id="sign-in">
                 <label class="field"><span class="dim">username</span>
                   <input id="username" autocomplete="username" autofocus /></label>
                 <label class="field"><span class="dim">code</span>
                   <input id="code" class="mono" placeholder="XXXXX-XXXXX"
                          autocomplete="one-time-code" /></label>
                 {error}
                 <button class="primary-button" {busy}>sign in</button>
                 <button type="button" id="passkey-sign-in" class="quiet-button" {busy}>
                   sign in with a passkey</button>
               </form>
             </div>
           </div>"#
    )
}

/// Attach listeners after a render. Listeners only dispatch — logic lives
/// in the reducer. (The small closures are forgotten on purpose: one per
/// render of a form, reclaimed with the document nodes.)
fn wire(document: &web_sys::Document) {
    if let Some(form) = document.get_element_by_id("sign-in") {
        let doc = document.clone();
        let on_submit = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            event.prevent_default();
            let value = |id: &str| {
                doc.get_element_by_id(id)
                    .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
                    .map(|i| i.value())
                    .unwrap_or_default()
            };
            dispatch(Action::SignInSubmitted {
                username: value("username"),
                code: value("code"),
            });
        });
        let _ = form.add_event_listener_with_callback("submit", on_submit.as_ref().unchecked_ref());
        on_submit.forget();
    }
    if let Some(button) = document.get_element_by_id("passkey-sign-in") {
        let doc = document.clone();
        let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            let username = doc
                .get_element_by_id("username")
                .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
                .map(|i| i.value())
                .unwrap_or_default();
            dispatch(Action::PasskeySignInRequested { username });
        });
        let _ = button.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
        on_click.forget();
    }
    if let Some(button) = document.get_element_by_id("enroll-passkey") {
        let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            dispatch(Action::EnrollPasskeyRequested);
        });
        let _ = button.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
        on_click.forget();
    }
    if let Ok(panes) = document.query_selector_all("[data-focus]") {
        for i in 0..panes.length() {
            if let Some(el) = panes
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            {
                let id = el.get_attribute("data-focus").unwrap_or_default();
                let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    dispatch(Action::Selected { id: id.clone() });
                });
                let _ =
                    el.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
                on_click.forget();
            }
        }
    }

    if let Some(input) = document
        .get_element_by_id("palette-input")
        .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
    {
        let _ = input.focus();
        // Put the caret at the end, not the start.
        let len = input.value().len() as u32;
        let _ = input.set_selection_range(len, len);
        let on_input = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            if let Some(target) = event
                .target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            {
                dispatch(Action::PaletteQueried {
                    query: target.value(),
                });
            }
        });
        let _ = input.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref());
        on_input.forget();
        let on_enter =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
                if e.key() == "Enter" {
                    e.prevent_default();
                    dispatch(Action::PaletteCommitted);
                }
            });
        let _ =
            input.add_event_listener_with_callback("keydown", on_enter.as_ref().unchecked_ref());
        on_enter.forget();
    }
    if let Some(well) = document
        .get_element_by_id("palette-args")
        .and_then(|e| e.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
    {
        let _ = well.focus();
        let target = well.clone();
        let on_enter =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
                if e.key() == "Enter" && !e.shift_key() {
                    e.prevent_default();
                    dispatch(Action::PaletteArgsCommitted {
                        draft: target.value(),
                    });
                }
            });
        let _ = well.add_event_listener_with_callback("keydown", on_enter.as_ref().unchecked_ref());
        on_enter.forget();
    }
    if let Ok(rows) = document.query_selector_all("[data-commit]") {
        for i in 0..rows.length() {
            if let Some(row) = rows
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            {
                let index: usize = row
                    .get_attribute("data-commit")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let on_click =
                    Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
                        event.stop_propagation();
                        // Land the selection on the clicked row, then
                        // commit — two ordinary dispatches, no special
                        // click path through the reducer.
                        let current = STATE
                            .with(|s| s.borrow().palette.as_ref().map(|p| p.selected))
                            .unwrap_or(0);
                        dispatch(Action::PaletteMoved {
                            delta: index as i32 - current as i32,
                        });
                        dispatch(Action::PaletteCommitted);
                    });
                let _ = row
                    .add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
                on_click.forget();
            }
        }
    }
    if let Ok(Some(scrim)) = document.query_selector("[data-dismiss-palette]") {
        let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            if event
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .is_some_and(|el| el.has_attribute("data-dismiss-palette"))
            {
                dispatch(Action::PaletteDismissed);
            }
        });
        let _ = scrim.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
        on_click.forget();
    }

    // Tree rows and create buttons: data attributes carry the action's
    // argument; the listener only dispatches.
    if let Ok(rows) = document.query_selector_all("[data-open]") {
        for i in 0..rows.length() {
            if let Some(row) = rows
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            {
                let id = row.get_attribute("data-open").unwrap_or_default();
                let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    dispatch(Action::Selected { id: id.clone() });
                });
                let _ = row
                    .add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
                on_click.forget();
            }
        }
    }
    if let Ok(titles) = document.query_selector_all("[data-rename]") {
        for i in 0..titles.length() {
            if let Some(el) = titles
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            {
                let id = el.get_attribute("data-rename").unwrap_or_default();
                let on_click =
                    Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
                        event.stop_propagation();
                        dispatch(Action::RenameStarted { id: id.clone() });
                    });
                let _ =
                    el.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
                on_click.forget();
            }
        }
    }
    if let Some(input) = document
        .get_element_by_id("rename-title")
        .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
    {
        let _ = input.focus();
        let len = input.value().len() as u32;
        let _ = input.set_selection_range(len, len);
        let on_input = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            if let Some(target) = event
                .target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            {
                dispatch(Action::RenameDrafted {
                    draft: target.value(),
                });
            }
        });
        let _ = input.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref());
        on_input.forget();
        let on_key =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
                match e.key().as_str() {
                    "Enter" => {
                        e.prevent_default();
                        if let Some(target) = e
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                        {
                            dispatch(Action::RenameCommitted {
                                title: target.value(),
                            });
                        }
                    }
                    "Escape" => {
                        e.prevent_default();
                        dispatch(Action::RenameDismissed);
                    }
                    _ => {}
                }
            });
        let _ = input.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());
        on_key.forget();
    }
    if let Ok(buttons) = document.query_selector_all("[data-create]") {
        for i in 0..buttons.length() {
            if let Some(button) = buttons
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            {
                let kind = button.get_attribute("data-create").unwrap_or_default();
                let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    dispatch(Action::CreateRequested { kind: kind.clone() });
                });
                let _ = button
                    .add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
                on_click.forget();
            }
        }
    }
    for (attr, make) in [
        (
            "data-take",
            (|id: String| Action::TakeRequested { id }) as fn(String) -> Action,
        ),
        ("data-release", |id| Action::ReleaseRequested { id }),
        ("data-close", |id| Action::PaneClosed { id }),
    ] {
        if let Ok(nodes) = document.query_selector_all(&format!("[{attr}]")) {
            for i in 0..nodes.length() {
                if let Some(el) = nodes
                    .item(i)
                    .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
                {
                    let id = el.get_attribute(attr).unwrap_or_default();
                    let on_click =
                        Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
                            // Pane buttons live inside clickable chrome;
                            // don't also select the row behind them.
                            event.stop_propagation();
                            dispatch(make(id.clone()));
                        });
                    let _ = el.add_event_listener_with_callback(
                        "click",
                        on_click.as_ref().unchecked_ref(),
                    );
                    on_click.forget();
                }
            }
        }
    }
    if let Ok(headers) = document.query_selector_all("[data-project]") {
        for i in 0..headers.length() {
            if let Some(el) = headers
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            {
                let project = el.get_attribute("data-project").unwrap_or_default();
                let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    dispatch(Action::ProjectSelected {
                        project: project.clone(),
                    });
                });
                let _ =
                    el.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
                on_click.forget();
            }
        }
    }
    if let Some(button) = document.get_element_by_id("new-project") {
        let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            dispatch(Action::NewProjectRequested);
        });
        let _ = button.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
        on_click.forget();
    }
    if let Some(input) = document
        .get_element_by_id("project-slug")
        .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
    {
        let _ = input.focus();
        let on_input = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            if let Some(target) = event
                .target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            {
                dispatch(Action::ProjectDrafted {
                    draft: target.value(),
                });
            }
        });
        let _ = input.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref());
        on_input.forget();
        let on_key =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
                match e.key().as_str() {
                    "Enter" => {
                        e.prevent_default();
                        if let Some(target) = e
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                        {
                            dispatch(Action::NewProjectCommitted {
                                slug: target.value(),
                            });
                        }
                    }
                    "Escape" => {
                        e.prevent_default();
                        dispatch(Action::ProjectSelected {
                            project: STATE.with(|s| s.borrow().workspace.current_project.clone()),
                        });
                    }
                    _ => {}
                }
            });
        let _ = input.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());
        on_key.forget();
    }
    if let Some(button) = document.get_element_by_id("sign-out") {
        let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            dispatch(Action::SignOutRequested);
        });
        let _ = button.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
        on_click.forget();
    }
}

// ---------------------------------------------------------------------------
// The workspace: sidebar tree + (for now) an empty stage
// ---------------------------------------------------------------------------

fn workspace_view(state: &State, user: &crate::core::User) -> String {
    let ws = &state.workspace;
    let feed = match ws.feed {
        crate::core::Feed::Live => r#"<span class="status-dot ok"></span><span>live</span>"#,
        crate::core::Feed::Connecting => {
            r#"<span class="status-dot"></span><span>connecting…</span>"#
        }
        crate::core::Feed::Reconnecting => {
            r#"<span class="status-dot attn"></span>
               <span>reconnecting — showing last state</span>"#
        }
    };

    // Group by project; the empty project reads as "workspace".
    let mut groups: Vec<(&str, Vec<&crate::core::InstanceInfo>)> = Vec::new();
    for instance in &ws.instances {
        let name = if instance.project.is_empty() {
            "workspace"
        } else {
            &instance.project
        };
        match groups.iter_mut().find(|(g, _)| *g == name) {
            Some((_, list)) => list.push(instance),
            None => groups.push((name, vec![instance])),
        }
    }
    let tree: String = groups
        .iter()
        .map(|(project, list)| {
            let value = if *project == "workspace" { "" } else { project };
            let current = if ws.current_project == value {
                " current"
            } else {
                ""
            };
            format!(
                r#"<div class="tree-project{current}" data-project="{value}">{label}</div>{rows}"#,
                value = escape(value),
                label = escape(project),
                rows = tree_rows(list, ws, state.renaming.as_ref()),
            )
        })
        .collect();
    let creates: String = ws
        .kinds
        .iter()
        .map(|k| {
            format!(
                r#"<button class="quiet-button" data-create="{kind}" title="{doc}">+ {kind}</button>"#,
                kind = escape(&k.kind),
                doc = escape(&k.doc),
            )
        })
        .collect();
    let chip = project_chip(ws);

    let overlay = palette_overlay(state);
    let notice = match &state.notice {
        Some(notice) if state.palette.is_none() => format!(
            r#"<div class="island notice mono">{}</div>"#,
            escape(notice)
        ),
        _ => String::new(),
    };
    format!(
        r#"{overlay}{notice}<div class="workspace">
             <div class="island sidebar">
               <div class="shell-brand"><span class="spore">●</span> myco</div>
               {chip}
               <div class="tree">{tree}</div>
               <div class="row-buttons creates">{creates}</div>
               <div class="sidebar-foot">
                 <span class="dim">{user}</span>
                 <div class="row-buttons">
                   <button id="enroll-passkey" class="quiet-button">add a passkey</button>
                   <button id="sign-out" class="quiet-button">sign out</button>
                 </div>
               </div>
               {note}
               <div class="status-line">{feed}</div>
             </div>
             <div class="stage{split}">{stage}</div>
           </div>"#,
        user = escape(user.display()),
        note = match &state.passkey_note {
            Some(note) => format!(r#"<div class="dim passkey-note">{}</div>"#, escape(note)),
            None => String::new(),
        },
        split = if ws.panes.is_empty() { "" } else { " split" },
        stage = if ws.panes.is_empty() {
            r#"<div class="dim stage-empty">open an instance from the tree</div>"#.to_string()
        } else {
            ws.panes
                .iter()
                .map(|p| pane_view(p, ws, state.renaming.as_ref()))
                .collect()
        },
    )
}

/// One pane: an island with chrome (title, the seat chip, close) over the
/// generic projection. The chip is STYLE.md's vocabulary: who drives, in
/// their hue; an open seat invites the take.
fn pane_view(
    pane: &crate::core::Pane,
    ws: &crate::core::Workspace,
    renaming: Option<&crate::core::RenameEdit>,
) -> String {
    let instance = ws.instances.iter().find(|i| i.id == pane.id);
    let title = instance
        .map(|i| {
            if i.title.is_empty() {
                i.kind.clone()
            } else {
                i.title.clone()
            }
        })
        .unwrap_or_else(|| pane.id.clone());
    let seat = crate::core::seat_of(instance.and_then(|i| i.driver.as_ref()));
    let chip = match &seat {
        crate::core::Seat::Open => format!(
            r#"<button class="chip open" data-take="{id}">seat open — take</button>"#,
            id = escape(&pane.id),
        ),
        crate::core::Seat::System => {
            format!(r#"<span class="chip system">{}</span>"#, seat.phrase())
        }
        held => format!(
            r#"<span class="chip {tone}">{who}</span>
               <button class="quiet-button" data-take="{id}">take</button>"#,
            tone = held.tone(),
            who = escape(&held.phrase()),
            id = escape(&pane.id),
        ),
    };
    let release = if seat == crate::core::Seat::Open {
        String::new()
    } else {
        format!(
            r#"<button class="quiet-button" data-release="{id}">release</button>"#,
            id = escape(&pane.id)
        )
    };
    let body = if pane.gone {
        r#"<div class="dim">gone — the instance was removed. last state below.</div>"#.to_string()
    } else {
        String::new()
    };
    let view = match &pane.view {
        Some(view) if pane.kind == "tty" => tty_screen(view),
        Some(view) => format!(
            r#"<pre class="mono pane-body">{}</pre>"#,
            escape(&pretty(view))
        ),
        None => r#"<div class="dim">reading…</div>"#.to_string(),
    };
    let focused = ws.selected.as_deref() == Some(pane.id.as_str());
    format!(
        r#"<div class="island pane {kind}{gone}{focus}" data-focus="{id}">
             <div class="pane-header">
               {title_node}
               <span class="pane-chrome">{chip}{release}
                 <button class="quiet-button" data-close="{id}">close</button></span>
             </div>
             {body}{view}
           </div>"#,
        kind = escape(&pane.kind),
        gone = if pane.gone { " pane-gone" } else { "" },
        focus = if focused { " focused" } else { "" },
        title_node = title_node(&pane.id, &title, renaming, true),
        id = escape(&pane.id),
    )
}

fn pretty(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| raw.to_string())
}

/// The terminal renderer: the tty's `screen` payload (styled runs, cursor)
/// as DOM, in the theme-constant graphite material. Rows are absolute; a
/// run's cells become one span with inline colors (already concrete
/// `#rrggbb` server-side).
fn tty_screen(raw: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Run {
        row: u16,
        text: String,
        #[serde(default)]
        fg: Option<String>,
        #[serde(default)]
        bg: Option<String>,
        #[serde(default)]
        bold: bool,
        #[serde(default)]
        italic: bool,
        #[serde(default)]
        underline: bool,
        #[serde(default)]
        inverse: bool,
        #[serde(default)]
        cursor: bool,
    }
    #[derive(serde::Deserialize)]
    struct Screen {
        rows: u16,
        #[serde(default)]
        cursor_hidden: bool,
        #[serde(default)]
        runs: Vec<Run>,
    }
    let Ok(screen) = serde_json::from_str::<Screen>(raw) else {
        return format!(
            r#"<pre class="mono pane-body">{}</pre>"#,
            escape(&pretty(raw))
        );
    };
    let mut rows: Vec<String> = (0..screen.rows).map(|_| String::new()).collect();
    for run in &screen.runs {
        let Some(row) = rows.get_mut(run.row as usize) else {
            continue;
        };
        let mut style = String::new();
        let (fg, bg) = if run.inverse {
            (run.bg.as_deref(), run.fg.as_deref())
        } else {
            (run.fg.as_deref(), run.bg.as_deref())
        };
        if let Some(fg) = fg
            && ok_color(fg)
        {
            style.push_str(&format!("color:{fg};"));
        }
        if run.inverse && bg.is_none() {
            style.push_str("background:#d8d5e0;color:#161420;");
        } else if let Some(bg) = bg
            && ok_color(bg)
        {
            style.push_str(&format!("background:{bg};"));
        }
        if run.bold {
            style.push_str("font-weight:700;");
        }
        if run.italic {
            style.push_str("font-style:italic;");
        }
        if run.underline {
            style.push_str("text-decoration:underline;");
        }
        let cursor = if run.cursor && !screen.cursor_hidden {
            " tty-cursor"
        } else {
            ""
        };
        row.push_str(&format!(
            r#"<span class="{cursor}" style="{style}">{}</span>"#,
            escape(&run.text),
        ));
    }
    let body: String = rows
        .into_iter()
        .map(|row| format!(r#"<div class="tty-row">{row}</div>"#))
        .collect();
    format!(r#"<div class="pane-body tty-screen mono" data-tty-screen>{body}</div>"#)
}

/// Only concrete `#rrggbb` colors reach inline styles — the payload is
/// server-shaped, but the render layer still refuses to interpolate
/// anything else into CSS.
fn ok_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

/// The current-project chip: click a tree header to set it, or `+ project`
/// to name a new one. Creating a project is just setting current — the
/// next `+ kind` writes it.
fn project_chip(ws: &crate::core::Workspace) -> String {
    match &ws.project_draft {
        Some(draft) => format!(
            r#"<input id="project-slug" class="mono project-slug" placeholder="project-slug" value="{draft}" />"#,
            draft = escape(draft),
        ),
        None => {
            let label = if ws.current_project.is_empty() {
                "workspace"
            } else {
                &ws.current_project
            };
            format!(
                r#"<div class="project-bar">
                     <button class="chip project" data-project="{value}" title="creates land here">{label}</button>
                     <button id="new-project" class="quiet-button">+ project</button>
                   </div>"#,
                value = escape(&ws.current_project),
                label = escape(label),
            )
        }
    }
}

/// How deep the tree will indent before it gives up. Parentage is acyclic
/// by construction at L1, so this can only ever fire on a server the client
/// should not have trusted — and a bounded lie renders better than a hang.
const MAX_TREE_DEPTH: usize = 8;

/// One project's rows: roots first, each followed by whatever hangs under
/// it. A row whose parent is not in this group renders as a root, because
/// an indent under nothing is a lie.
fn tree_rows(
    list: &[&crate::core::InstanceInfo],
    ws: &crate::core::Workspace,
    renaming: Option<&crate::core::RenameEdit>,
) -> String {
    let mut out = String::new();
    for instance in list {
        let orphan = instance
            .parent
            .as_deref()
            .is_none_or(|p| !list.iter().any(|i| i.id == p));
        if orphan {
            out.push_str(&tree_row(instance, ws, 0, renaming));
            push_children(&mut out, list, &instance.id, ws, 1, renaming);
        }
    }
    out
}

fn push_children(
    out: &mut String,
    list: &[&crate::core::InstanceInfo],
    parent: &str,
    ws: &crate::core::Workspace,
    depth: usize,
    renaming: Option<&crate::core::RenameEdit>,
) {
    if depth > MAX_TREE_DEPTH {
        return;
    }
    for child in list.iter().filter(|i| i.parent.as_deref() == Some(parent)) {
        out.push_str(&tree_row(child, ws, depth, renaming));
        push_children(out, list, &child.id, ws, depth + 1, renaming);
    }
}

/// One tree row: presence dot for the seat, kind glyph, title, indented by
/// its parentage. An open seat is an open ring (the STYLE.md vocabulary);
/// crashed dims the row.
fn tree_row(
    instance: &crate::core::InstanceInfo,
    ws: &crate::core::Workspace,
    depth: usize,
    renaming: Option<&crate::core::RenameEdit>,
) -> String {
    let selected = ws.selected.as_deref() == Some(instance.id.as_str());
    let title = if instance.title.is_empty() {
        &instance.kind
    } else {
        &instance.title
    };
    format!(
        r#"<div class="tree-row{sel}{crashed}" data-open="{id}" style="--depth:{depth}">
             <span class="seat {seat}"></span>
             <span class="kind-tag mono">{kind}</span>
             {title_node}
           </div>"#,
        sel = if selected { " selected" } else { "" },
        crashed = if instance.crashed { " crashed" } else { "" },
        id = escape(&instance.id),
        seat = crate::core::seat_of(instance.driver.as_ref()).tone(),
        kind = escape(&instance.kind),
        title_node = title_node(
            &instance.id,
            title,
            renaming,
            !ws.panes.iter().any(|p| p.id == instance.id),
        ),
    )
}

/// Click the selected title to rename; a bad slug stays in the field.
fn title_node(
    id: &str,
    title: &str,
    renaming: Option<&crate::core::RenameEdit>,
    allow_input: bool,
) -> String {
    match renaming {
        Some(edit) if edit.id == id && allow_input => {
            let err = match &edit.error {
                Some(why) => format!(r#"<span class="rename-error">{}</span>"#, escape(why)),
                None => String::new(),
            };
            format!(
                r#"<span class="rename-wrap"><input id="rename-title" class="mono rename-input" value="{draft}" />{err}</span>"#,
                draft = escape(&edit.draft),
            )
        }
        Some(edit) if edit.id == id => {
            format!(r#"<span class="row-title">{}</span>"#, escape(&edit.draft),)
        }
        _ => format!(
            r#"<span class="row-title" data-rename="{id}">{title}</span>"#,
            id = escape(id),
            title = escape(title),
        ),
    }
}

/// The summoned palette: one island over a scrim, the amethyst spec —
/// grouped rows, gated rows visible with their reason, the args well as
/// the second stage in the same island.
fn palette_overlay(state: &State) -> String {
    let Some(palette) = &state.palette else {
        return String::new();
    };
    let inner = match &palette.stage {
        Stage::List => {
            let rows = palette_rows(state, &palette.query);
            let mut html = format!(
                r#"<input id="palette-input" placeholder="type a verb, an instance, a kind…"
                     value="{}" autocomplete="off" />"#,
                escape(&palette.query)
            );
            let mut group = "";
            for (i, row) in rows.iter().enumerate() {
                if row.group != group {
                    group = row.group;
                    html.push_str(&format!(r#"<div class="palette-group">{group}</div>"#));
                }
                let classes = format!(
                    "palette-row{}{}",
                    if i == palette.selected {
                        " selected"
                    } else {
                        ""
                    },
                    if row.gated.is_some() { " gated" } else { "" },
                );
                let right = match &row.gated {
                    Some(reason) => {
                        format!(r#"<span class="palette-gate">{}</span>"#, escape(reason))
                    }
                    None => format!(
                        r#"<span class="palette-detail">{}</span>"#,
                        escape(&row.detail)
                    ),
                };
                html.push_str(&format!(
                    r#"<div class="{classes}" data-commit="{i}">
                         <span class="mono">{label}</span>{right}
                       </div>"#,
                    label = escape(&row.label),
                ));
            }
            if rows.is_empty() {
                html.push_str(r#"<div class="dim">no matches — try a kind name</div>"#);
            }
            html
        }
        Stage::Args {
            verb, draft, error, ..
        } => {
            let error = match error {
                Some(why) => format!(r#"<div class="form-error">{}</div>"#, escape(why)),
                None => String::new(),
            };
            format!(
                r#"<div class="dim"><span class="mono">{verb}</span> wants arguments — JSON,
                     enter to run, esc to go back</div>
                   {error}
                   <textarea id="palette-args" class="mono" rows="4">{draft}</textarea>"#,
                verb = escape(verb),
                draft = escape(draft),
            )
        }
    };
    format!(
        r#"<div class="palette-scrim" data-dismiss-palette>
             <div class="island palette" id="palette">{inner}</div>
           </div>"#
    )
}

/// Measure the focused tty pane's cell grid and offer it to the reducer,
/// which resizes at most once per distinct size — so render → measure →
/// dispatch cannot spin (the repeat measurement produces no effect and no
/// state change worth re-rendering... except the action log; hence the
/// edge-side dedupe here too).
fn measure_focused_tty(document: &web_sys::Document, id: Option<String>) {
    thread_local! {
        static LAST: RefCell<Option<(String, u16, u16)>> = const { RefCell::new(None) };
    }
    let Some(id) = id else {
        return;
    };
    let Some(el) = document
        .query_selector(".pane.focused [data-tty-screen]")
        .ok()
        .flatten()
    else {
        return;
    };
    let Ok(el) = el.dyn_into::<web_sys::HtmlElement>() else {
        return;
    };
    // One character cell, measured off the live screen's font — not a
    // detached body span that can disagree with `.tty-screen`.
    let Some((cw, ch)) = char_cell(&el) else {
        return;
    };
    let cols = (el.client_width() as f64 / cw).floor().clamp(20.0, 500.0) as u16;
    let rows = (el.client_height() as f64 / ch).floor().clamp(5.0, 200.0) as u16;
    let fresh = LAST.with(|last| {
        let mut last = last.borrow_mut();
        if *last == Some((id.clone(), cols, rows)) {
            false
        } else {
            *last = Some((id.clone(), cols, rows));
            true
        }
    });
    if fresh {
        dispatch(Action::PaneMeasured { id, cols, rows });
    }
}

fn char_cell(screen: &web_sys::HtmlElement) -> Option<(f64, f64)> {
    let document = screen.owner_document()?;
    let probe = document.create_element("span").ok()?;
    probe.set_class_name("tty-probe");
    probe.set_text_content(Some("MMMMMMMMMM"));
    screen.append_child(&probe).ok()?;
    let rect = probe.get_bounding_client_rect();
    let (w, h) = (rect.width() / 10.0, rect.height());
    probe.remove();
    (w > 0.0 && h > 0.0).then_some((w, h))
}

/// Text into markup, safely. Server-controlled strings still get escaped —
/// the render layer trusts nothing it did not write.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
