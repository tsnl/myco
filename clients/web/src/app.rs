//! The wasm edge: dispatch, render, effects. An `onClick` is a dispatch;
//! an effect's completion is a dispatch; nothing here holds state or
//! contains logic — the reducer is the whole brain.

use std::cell::RefCell;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::core::{Action, Effect, Session, State, reduce};

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
    dispatch(Action::Boot {
        token: storage().and_then(|s| s.get_item(TOKEN_KEY).ok().flatten()),
    });
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
            dispatch(match fetch("GET", "/api/instances", Some(&token), None).await {
                Ok((status, body)) => Action::InstancesAnswered { status, body },
                Err(what) => Action::NetworkFailed { what },
            });
        }),
        Effect::OpenFeed { token } => open_feed(token),
        Effect::CreateInstance { token, kind } => wasm_bindgen_futures::spawn_local(async move {
            let body = serde_json::json!({ "kind": kind }).to_string();
            dispatch(
                match fetch("POST", "/api/instances", Some(&token), Some((JSON, body))).await {
                    Ok((status, body)) => Action::CreateAnswered { status, body },
                    Err(what) => Action::NetworkFailed { what },
                },
            );
        }),
        Effect::Watch { id } => send_op("watch", &id),
        Effect::Unwatch { id } => send_op("unwatch", &id),
        Effect::ReadPane { token, id, verb } => wasm_bindgen_futures::spawn_local(async move {
            let url = format!("/api/instances/{id}/verbs/{verb}");
            dispatch(match fetch("POST", &url, Some(&token), None).await {
                Ok((status, body)) => Action::PaneRead { id, status, body },
                Err(what) => Action::NetworkFailed { what },
            });
        }),
        Effect::CallVerb { token, id, verb } => wasm_bindgen_futures::spawn_local(async move {
            let url = format!("/api/instances/{id}/verbs/{verb}");
            dispatch(match fetch("POST", &url, Some(&token), None).await {
                Ok((status, _body)) => Action::VerbAnswered { id, verb, status },
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
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                retry.unchecked_ref(),
                2000,
            );
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
    let (status, challenge) =
        match fetch("POST", "/api/auth/passkey/register/start", Some(token), None).await {
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
    js_sys::encode_uri_component(text).as_string().unwrap_or_default()
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
        let _ = form
            .add_event_listener_with_callback("submit", on_submit.as_ref().unchecked_ref());
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
    // Tree rows and create buttons: data attributes carry the action's
    // argument; the listener only dispatches.
    if let Ok(rows) = document.query_selector_all("[data-open]") {
        for i in 0..rows.length() {
            if let Some(row) = rows.item(i).and_then(|n| n.dyn_into::<web_sys::Element>().ok()) {
                let id = row.get_attribute("data-open").unwrap_or_default();
                let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    dispatch(Action::Selected { id: id.clone() });
                });
                let _ =
                    row.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
                on_click.forget();
            }
        }
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
        ("data-take", (|id: String| Action::TakeRequested { id }) as fn(String) -> Action),
        ("data-release", |id| Action::ReleaseRequested { id }),
        ("data-close", |id| Action::PaneClosed { id }),
    ] {
        if let Ok(nodes) = document.query_selector_all(&format!("[{attr}]")) {
            for i in 0..nodes.length() {
                if let Some(el) = nodes.item(i).and_then(|n| n.dyn_into::<web_sys::Element>().ok())
                {
                    let id = el.get_attribute(attr).unwrap_or_default();
                    let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(
                        move |event: web_sys::Event| {
                            // Pane buttons live inside clickable chrome;
                            // don't also select the row behind them.
                            event.stop_propagation();
                            dispatch(make(id.clone()));
                        },
                    );
                    let _ = el.add_event_listener_with_callback(
                        "click",
                        on_click.as_ref().unchecked_ref(),
                    );
                    on_click.forget();
                }
            }
        }
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
            let rows: String = list.iter().map(|i| tree_row(i, ws)).collect();
            format!(
                r#"<div class="tree-project">{}</div>{rows}"#,
                escape(project)
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

    format!(
        r#"<div class="workspace">
             <div class="island sidebar">
               <div class="shell-brand"><span class="spore">●</span> myco</div>
               <div class="tree">{tree}</div>
               <div class="row-buttons creates">{creates}</div>
               <div class="sidebar-foot">
                 <span class="dim">{user}</span>
                 <button id="sign-out" class="quiet-button">sign out</button>
               </div>
               <div class="status-line">{feed}</div>
             </div>
             <div class="stage{split}">{stage}</div>
           </div>"#,
        user = escape(user.display()),
        split = if ws.panes.is_empty() { "" } else { " split" },
        stage = if ws.panes.is_empty() {
            r#"<div class="dim stage-empty">open an instance from the tree</div>"#.to_string()
        } else {
            ws.panes.iter().map(|p| pane_view(p, ws)).collect()
        },
    )
}

/// One pane: an island with chrome (title, the seat chip, close) over the
/// generic projection. The chip is STYLE.md's vocabulary: who drives, in
/// their hue; an open seat invites the take.
fn pane_view(pane: &crate::core::Pane, ws: &crate::core::Workspace) -> String {
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
    let chip = match instance.and_then(|i| i.driver.as_ref()) {
        Some(p) if p.kind == "human" => format!(
            r#"<span class="chip human">{} driving</span>
               <button class="quiet-button" data-take="{id}">take</button>"#,
            escape(&p.id),
            id = escape(&pane.id),
        ),
        Some(p) if p.kind == "agent" => format!(
            r#"<span class="chip agent">agent driving</span>
               <button class="quiet-button" data-take="{id}">take</button>"#,
            id = escape(&pane.id),
        ),
        Some(_) => r#"<span class="chip system">system driving</span>"#.to_string(),
        None => format!(
            r#"<button class="chip open" data-take="{id}">seat open — take</button>"#,
            id = escape(&pane.id),
        ),
    };
    let release = match instance.and_then(|i| i.driver.as_ref()) {
        Some(_) => format!(
            r#"<button class="quiet-button" data-release="{id}">release</button>"#,
            id = escape(&pane.id)
        ),
        None => String::new(),
    };
    let body = if pane.gone {
        r#"<div class="dim">gone — the instance was removed. last state below.</div>"#.to_string()
    } else {
        String::new()
    };
    let view = match &pane.view {
        Some(view) => format!(r#"<pre class="mono pane-body">{}</pre>"#, escape(view)),
        None => r#"<div class="dim">reading…</div>"#.to_string(),
    };
    format!(
        r#"<div class="island pane{gone}">
             <div class="pane-header">
               <span class="row-title">{title}</span>
               <span class="pane-chrome">{chip}{release}
                 <button class="quiet-button" data-close="{id}">close</button></span>
             </div>
             {body}{view}
           </div>"#,
        gone = if pane.gone { " pane-gone" } else { "" },
        title = escape(&title),
        id = escape(&pane.id),
    )
}

/// One tree row: presence dot for the seat, kind glyph, title. An open
/// seat is an open ring (the STYLE.md vocabulary); crashed dims the row.
fn tree_row(instance: &crate::core::InstanceInfo, ws: &crate::core::Workspace) -> String {
    let selected = ws.selected.as_deref() == Some(instance.id.as_str());
    let seat = match &instance.driver {
        Some(p) if p.kind == "human" => "seat human",
        Some(p) if p.kind == "agent" => "seat agent",
        Some(_) => "seat system",
        None => "seat open",
    };
    let title = if instance.title.is_empty() {
        &instance.kind
    } else {
        &instance.title
    };
    format!(
        r#"<div class="tree-row{sel}{crashed}" data-open="{id}">
             <span class="{seat}"></span>
             <span class="kind-tag mono">{kind}</span>
             <span class="row-title">{title}</span>
           </div>"#,
        sel = if selected { " selected" } else { "" },
        crashed = if instance.crashed { " crashed" } else { "" },
        id = escape(&instance.id),
        kind = escape(&instance.kind),
        title = escape(title),
    )
}

/// Text into markup, safely. Server-controlled strings still get escaped —
/// the render layer trusts nothing it did not write.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
