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

    let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |_| {
        dispatch(Action::FeedEvent);
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
                rows = tree_rows(list, ws),
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

    format!(
        r#"<div class="workspace">
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
             <div class="stage">
               <div class="dim">{stage}</div>
             </div>
           </div>"#,
        user = escape(user.display()),
        note = match &state.passkey_note {
            Some(note) => format!(r#"<div class="dim passkey-note">{}</div>"#, escape(note)),
            None => String::new(),
        },
        stage = match &ws.selected {
            Some(id) => format!("selected {} — panes arrive with the next PR", escape(id)),
            None => "open an instance from the tree".to_string(),
        },
    )
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
fn tree_rows(list: &[&crate::core::InstanceInfo], ws: &crate::core::Workspace) -> String {
    let mut out = String::new();
    for instance in list {
        let orphan = instance
            .parent
            .as_deref()
            .is_none_or(|p| !list.iter().any(|i| i.id == p));
        if orphan {
            out.push_str(&tree_row(instance, ws, 0));
            push_children(&mut out, list, &instance.id, ws, 1);
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
) {
    if depth > MAX_TREE_DEPTH {
        return;
    }
    for child in list.iter().filter(|i| i.parent.as_deref() == Some(parent)) {
        out.push_str(&tree_row(child, ws, depth));
        push_children(out, list, &child.id, ws, depth + 1);
    }
}

/// One tree row: presence dot for the seat, kind glyph, title, indented by
/// its parentage. An open seat is an open ring (the STYLE.md vocabulary);
/// crashed dims the row.
fn tree_row(
    instance: &crate::core::InstanceInfo,
    ws: &crate::core::Workspace,
    depth: usize,
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
             <span class="row-title">{title}</span>
           </div>"#,
        sel = if selected { " selected" } else { "" },
        crashed = if instance.crashed { " crashed" } else { "" },
        id = escape(&instance.id),
        seat = crate::core::seat_of(instance.driver.as_ref()).tone(),
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
