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
    }
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
        Session::SignedIn(user) => {
            let note = match &state.passkey_note {
                Some(note) => format!(r#"<div class="dim">{}</div>"#, escape(note)),
                None => String::new(),
            };
            shell_card(
                "ok",
                &format!("signed in as {}", escape(user.display())),
                &format!(
                    r#"<div class="row-buttons">
                         <button id="enroll-passkey" class="quiet-button">add a passkey</button>
                         <button id="sign-out" class="quiet-button">sign out</button>
                       </div>
                       {note}
                       <div class="dim mono">the workspace arrives with the next PRs</div>"#
                ),
            )
        }
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
    if let Some(button) = document.get_element_by_id("sign-out") {
        let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            dispatch(Action::SignOutRequested);
        });
        let _ = button.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
        on_click.forget();
    }
}

/// Text into markup, safely. Server-controlled strings still get escaped —
/// the render layer trusts nothing it did not write.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
