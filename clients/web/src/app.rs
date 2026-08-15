//! The wasm edge: dispatch, render, effects. An `onClick` is a dispatch;
//! an effect's completion is a dispatch; nothing here holds state or
//! contains logic — the reducer is the whole brain.

use std::cell::RefCell;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::core::{
    Action, AdminAct, Effect, Session, Stage, State, palette_rows, reduce, reserved_chord,
    wants_key,
};

const TOKEN_KEY: &str = "myco.token";

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    /// The markup each region last received. A region is rewritten only
    /// when its markup differs from this — the whole of dirty rendering.
    static PAINTED: RefCell<std::collections::HashMap<String, String>> =
        RefCell::new(std::collections::HashMap::new());
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
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        attach_delegates(&document);
    }
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
            // A field being typed into owns its own Enter. Everything else
            // about typing is ordinary DOM; only the commit is ours.
            if let Some(field) = event
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                && typed_in_field(&field, &key, event.shift_key())
            {
                event.prevent_default();
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

/// Enter in a field: the palette's input runs the selected row, its args
/// well runs the drafted JSON, a composer sends (shift-enter is a
/// newline). Escape dismisses an inline rename or a project draft.
/// Answers whether the keystroke was spent here.
fn typed_in_field(field: &web_sys::Element, key: &str, shift: bool) -> bool {
    let id = field.id();
    if key == "Escape" {
        if id == "rename-title" {
            dispatch(Action::RenameDismissed);
            return true;
        }
        if id == "project-slug" {
            dispatch(Action::ProjectSelected {
                project: STATE.with(|s| s.borrow().workspace.current_project.clone()),
            });
            return true;
        }
        return false;
    }
    if key != "Enter" {
        return false;
    }
    if id == "palette-input" {
        dispatch(Action::PaletteCommitted);
        return true;
    }
    if shift {
        return false;
    }
    if id == "palette-args" {
        dispatch(Action::PaletteArgsCommitted {
            draft: field_value("palette-args"),
        });
        return true;
    }
    if id == "rename-title" {
        dispatch(Action::RenameCommitted {
            title: field_value("rename-title"),
        });
        return true;
    }
    if id == "project-slug" {
        dispatch(Action::NewProjectCommitted {
            slug: field_value("project-slug"),
        });
        return true;
    }
    if let Some(chat) = id.strip_prefix("composer-") {
        let text = field_value(&id);
        clear_field(&id);
        dispatch(Action::ChatPosted {
            id: chat.to_string(),
            text,
        });
        return true;
    }
    false
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
            args,
        } => wasm_bindgen_futures::spawn_local(async move {
            let args: serde_json::Value =
                serde_json::from_str(&args).unwrap_or(serde_json::json!({}));
            let body = serde_json::json!({
                "kind": kind,
                "project": project,
                "title": title,
                "args": args,
            })
            .to_string();
            dispatch(
                match fetch("POST", "/api/instances", Some(&token), Some((JSON, body))).await {
                    Ok((status, body)) => Action::CreateAnswered { kind, status, body },
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
        Effect::FetchAdmin { token } => wasm_bindgen_futures::spawn_local(async move {
            dispatch(
                match fetch("GET", "/api/admin/users", Some(&token), None).await {
                    Ok((status, body)) => Action::AdminAnswered { status, body },
                    Err(what) => Action::NetworkFailed { what },
                },
            );
        }),
        Effect::AdminAct { token, user, act } => wasm_bindgen_futures::spawn_local(async move {
            let (method, path) = match act {
                AdminAct::Mint => ("POST", format!("/api/admin/users/{user}/code")),
                AdminAct::Disable => ("POST", format!("/api/admin/users/{user}/disable")),
                AdminAct::Enable => ("POST", format!("/api/admin/users/{user}/enable")),
                AdminAct::Revoke => ("POST", format!("/api/admin/users/{user}/revoke")),
                AdminAct::ClearPasskeys => {
                    ("POST", format!("/api/admin/users/{user}/passkeys/clear"))
                }
                AdminAct::Remove => ("DELETE", format!("/api/admin/users/{user}")),
            };
            dispatch(match fetch(method, &path, Some(&token), None).await {
                Ok((status, body)) => Action::AdminActAnswered {
                    user,
                    act,
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

/// The room's fixed bones, written once. Every render after this replaces
/// the *contents* of a region, never the room — which is what makes an
/// unchanged region cost nothing and a focused field survive a render
/// somewhere else.
const SKELETON: &str = r#"<div id="region-overlay"></div>
<div id="region-notice"></div>
<div id="region-shell" class="shell-center"></div>
<div class="workspace" id="region-workspace">
  <div class="island sidebar" id="region-sidebar"></div>
  <div class="stage" id="region-stage"></div>
</div>"#;

/// Render is a pure function of state — and a *diff* against what is
/// already on screen.
///
/// The old shape rewrote `<body>` on every action, which meant every
/// keystroke destroyed the field being typed into and every feed event
/// destroyed the terminal being read. Each region's markup is formatted
/// (cheap: a string), compared with what that region last got, and written
/// only if it differs. Comparing the markup rather than the state slice it
/// came from is deliberate: it is the same test one step later, and it
/// cannot be wrong about which slices a region reads.
fn render(state: &State) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(body) = document.body() else {
        return;
    };
    if document.get_element_by_id("region-shell").is_none() {
        body.set_inner_html(SKELETON);
    }

    let signed_in = matches!(state.session, Session::SignedIn(_));
    paint(&document, "region-overlay", &{
        let mut html = palette_overlay(state);
        html.push_str(&admin_overlay(state));
        html
    });
    paint(&document, "region-notice", &notice_view(state));
    paint(
        &document,
        "region-shell",
        &match &state.session {
            Session::Checking => shell_card("", "reaching the server…", ""),
            Session::Unreachable { why } => shell_card("attn", &escape(why), ""),
            Session::SignedOut => sign_in_card(state),
            Session::SignedIn(_) => String::new(),
        },
    );
    if let Session::SignedIn(user) = &state.session {
        paint(&document, "region-sidebar", &sidebar_view(state, user));
        paint(&document, "region-stage", &stage_view(state));
    }
    // The two full-height regions are alternatives: one room at a time.
    show(&document, "region-shell", !signed_in);
    show(&document, "region-workspace", signed_in);
    if let Some(stage) = document.get_element_by_id("region-stage") {
        let split = if state.workspace.panes.is_empty() {
            "stage"
        } else {
            "stage split"
        };
        if stage.class_name() != split {
            stage.set_class_name(split);
        }
    }

    focus_summoned_field(&document);
    measure_focused_tty(&document);
}

/// Write a region's markup, but only when it actually changed — and put
/// the caret back when it did. Losing a caret mid-sentence is the whole
/// reason wholesale re-rendering was intolerable, so the one case where a
/// rewrite is unavoidable is the one case that restores it.
///
/// Chat transcripts are the other thing `set_inner_html` destroys: scroll
/// snaps to 0. If the user was at (or near) the bottom, pin them there
/// after the rewrite; if they had scrolled up, leave that offset. Skip
/// both when the markup is unchanged — a no-op paint must not fight them.
fn paint(document: &web_sys::Document, region: &str, html: &str) {
    let unchanged = PAINTED.with(|painted| {
        painted
            .borrow()
            .get(region)
            .is_some_and(|last| last == html)
    });
    if unchanged {
        return;
    }
    let Some(host) = document.get_element_by_id(region) else {
        return;
    };
    let caret = caret_in(document, &host);
    let pins = transcript_pins(&host);
    host.set_inner_html(html);
    restore_caret(document, caret);
    restore_transcript_pins(&host, &pins);
    PAINTED.with(|painted| {
        painted
            .borrow_mut()
            .insert(region.to_string(), html.to_string())
    });
}

/// Near-enough to the bottom that a new line should still pin. Matches
/// a typical chat "stick" slop — a hair more than one entry.
const STICK_SLOP: i32 = 32;

struct TranscriptPin {
    id: String,
    pin: bool,
    top: i32,
}

fn transcript_of(el: &web_sys::Element) -> Option<(String, web_sys::HtmlElement)> {
    let html = el.clone().dyn_into::<web_sys::HtmlElement>().ok()?;
    let id = el
        .closest("[data-focus]")
        .ok()
        .flatten()
        .and_then(|p| p.get_attribute("data-focus"))?;
    (!id.is_empty()).then_some((id, html))
}

fn transcript_pins(host: &web_sys::Element) -> Vec<TranscriptPin> {
    let Ok(list) = host.query_selector_all(".transcript") else {
        return Vec::new();
    };
    let mut pins = Vec::new();
    for i in 0..list.length() {
        let Some(node) = list.item(i) else {
            continue;
        };
        let Ok(el) = node.dyn_into::<web_sys::Element>() else {
            continue;
        };
        let Some((id, html)) = transcript_of(&el) else {
            continue;
        };
        let remaining = html.scroll_height() - html.scroll_top() - html.client_height();
        pins.push(TranscriptPin {
            id,
            pin: remaining <= STICK_SLOP,
            top: html.scroll_top(),
        });
    }
    pins
}

fn restore_transcript_pins(host: &web_sys::Element, pins: &[TranscriptPin]) {
    let Ok(list) = host.query_selector_all(".transcript") else {
        return;
    };
    for i in 0..list.length() {
        let Some(node) = list.item(i) else {
            continue;
        };
        let Ok(el) = node.dyn_into::<web_sys::Element>() else {
            continue;
        };
        let Some((id, html)) = transcript_of(&el) else {
            continue;
        };
        match pins.iter().find(|p| p.id == id) {
            Some(p) if p.pin => html.set_scroll_top(html.scroll_height()),
            Some(p) => html.set_scroll_top(p.top),
            // First paint of this chat: land at the latest message.
            None => html.set_scroll_top(html.scroll_height()),
        }
    }
}

/// A field mid-use, when it sits inside the region about to be rewritten:
/// what it holds and where the caret is. Only named fields are
/// restorable — an id is what survives `set_inner_html`. The *value* is
/// restored too, because a composer is uncontrolled by design (the DOM
/// holds the sentence until it is sent), so a repaint that dropped it
/// would drop what someone was in the middle of writing.
struct Caret {
    id: String,
    value: String,
    start: u32,
    end: u32,
}

fn caret_in(document: &web_sys::Document, host: &web_sys::Element) -> Option<Caret> {
    let active = document.active_element()?;
    if !host.contains(Some(&active)) || active.id().is_empty() {
        return None;
    }
    let (value, start, end) = if let Some(area) = active.dyn_ref::<web_sys::HtmlTextAreaElement>() {
        (
            area.value(),
            area.selection_start().ok()?,
            area.selection_end().ok()?,
        )
    } else if let Some(input) = active.dyn_ref::<web_sys::HtmlInputElement>() {
        (
            input.value(),
            input.selection_start().ok()?,
            input.selection_end().ok()?,
        )
    } else {
        return None;
    };
    Some(Caret {
        id: active.id(),
        value,
        start: start.unwrap_or(0),
        end: end.unwrap_or(0),
    })
}

fn restore_caret(document: &web_sys::Document, caret: Option<Caret>) {
    let Some(caret) = caret else { return };
    let Some(el) = document.get_element_by_id(&caret.id) else {
        return;
    };
    if let Some(html) = el.dyn_ref::<web_sys::HtmlElement>() {
        let _ = html.focus();
    }
    if let Some(area) = el.dyn_ref::<web_sys::HtmlTextAreaElement>() {
        if area.value() != caret.value {
            area.set_value(&caret.value);
        }
        let _ = area.set_selection_range(caret.start, caret.end);
    } else if let Some(input) = el.dyn_ref::<web_sys::HtmlInputElement>() {
        if input.value() != caret.value {
            input.set_value(&caret.value);
        }
        let _ = input.set_selection_range(caret.start, caret.end);
    }
}

/// Empty an uncontrolled field the edge owns — the composer, once its
/// sentence has been sent. The reducer clears its mirror; the DOM is ours.
fn clear_field(id: &str) {
    if let Some(area) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .and_then(|e| e.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
    {
        area.set_value("");
    }
}

/// `hidden` on the region that is not the current room. The two are
/// full-height alternatives, so leaving both in the layout would stack
/// them.
fn show(document: &web_sys::Document, region: &str, visible: bool) {
    let Some(el) = document.get_element_by_id(region) else {
        return;
    };
    if visible {
        let _ = el.remove_attribute("hidden");
    } else {
        let _ = el.set_attribute("hidden", "");
    }
}

/// A field that appears because it was summoned takes the caret: the
/// palette's input, or the args well it hands you. Restoration handles
/// every later render; this is the first one, where there was nothing to
/// restore from.
fn focus_summoned_field(document: &web_sys::Document) {
    let already = document
        .active_element()
        .map(|el| el.id())
        .unwrap_or_default();
    if already == "palette-args"
        || already == "palette-input"
        || already == "rename-title"
        || already == "project-slug"
    {
        return;
    }
    for id in ["rename-title", "project-slug", "palette-args", "palette-input"] {
        let Some(el) = document.get_element_by_id(id) else {
            continue;
        };
        if let Some(html) = el.dyn_ref::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
        // The caret goes to the end, not in front of what is already typed.
        if let Some(input) = el.dyn_ref::<web_sys::HtmlInputElement>() {
            let len = input.value().len() as u32;
            let _ = input.set_selection_range(len, len);
        }
        return;
    }
}

fn shell_card(dot: &str, line: &str, extra: &str) -> String {
    format!(
        r#"<div class="island shell-card">
             <div class="shell-brand"><span class="spore">●</span> myco</div>
             <div class="dim">a workspace of instances, shared by humans and agents</div>
             <div class="status-line"><span class="status-dot {dot}"></span><span>{line}</span></div>
             {extra}
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
        r#"<div class="island shell-card">
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
               <button type="button" data-act="passkey-sign-in" class="quiet-button" {busy}>
                 sign in with a passkey</button>
             </form>
           </div>"#
    )
}

/// The delegated listeners, attached once to the document.
///
/// The old shape re-attached a closure per element after every render and
/// leaked each one (`Closure::forget`) — a listener per tree row per
/// keystroke. These four are created at startup and never again: a click,
/// a submit, and an input anywhere in the room arrive here, and the
/// *nearest ancestor of the target carrying an action attribute* decides
/// what it meant. That is what a `stopPropagation` on every inner button
/// used to buy, except it cannot be forgotten on a button added later.
fn attach_delegates(document: &web_sys::Document) {
    let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        let Some(target) = event
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let mut node = Some(target);
        while let Some(el) = node {
            if clicked(&el) {
                return;
            }
            // An island swallows clicks on its own background, so the
            // scrim behind it does not read them as "dismiss".
            if el.has_attribute("data-keep") {
                return;
            }
            node = el.parent_element();
        }
    });
    let _ = document.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
    on_click.forget();

    let on_submit = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        let Some(form) = event
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        event.prevent_default();
        submitted(&form);
    });
    let _ = document.add_event_listener_with_callback("submit", on_submit.as_ref().unchecked_ref());
    on_submit.forget();

    let on_input = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        let Some(el) = event
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        if let Some(input) = el.dyn_ref::<web_sys::HtmlInputElement>() {
            match el.id().as_str() {
                "palette-input" => dispatch(Action::PaletteQueried {
                    query: input.value(),
                }),
                "rename-title" => dispatch(Action::RenameDrafted {
                    draft: input.value(),
                }),
                "project-slug" => dispatch(Action::ProjectDrafted {
                    draft: input.value(),
                }),
                _ => {}
            }
        }
    });
    let _ = document.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref());
    on_input.forget();
}

/// What one element's action attributes mean. `true` means "this element
/// answered the click", which stops the walk up the tree.
fn clicked(el: &web_sys::Element) -> bool {
    let attr = |name: &str| el.get_attribute(name);
    if let Some(id) = attr("data-open").or_else(|| attr("data-focus")) {
        dispatch(Action::Selected { id });
    } else if let Some(kind) = attr("data-create") {
        dispatch(Action::CreateRequested { kind });
    } else if let Some(id) = attr("data-take") {
        dispatch(Action::TakeRequested { id });
    } else if let Some(id) = attr("data-release") {
        dispatch(Action::ReleaseRequested { id });
    } else if let Some(id) = attr("data-close") {
        dispatch(Action::PaneClosed { id });
    } else if let Some(id) = attr("data-cancel") {
        dispatch(Action::TurnCancelled { id });
    } else if let Some(id) = attr("data-rename") {
        dispatch(Action::RenameStarted { id });
    } else if let Some(project) = attr("data-project") {
        dispatch(Action::ProjectSelected { project });
    } else if let Some(index) = attr("data-commit").and_then(|v| v.parse::<usize>().ok()) {
        // Land the selection on the clicked row, then commit — two
        // ordinary dispatches, no special click path through the reducer.
        let current = STATE
            .with(|s| s.borrow().palette.as_ref().map(|p| p.selected))
            .unwrap_or(0);
        dispatch(Action::PaletteMoved {
            delta: index as i32 - current as i32,
        });
        dispatch(Action::PaletteCommitted);
    } else if let (Some(name), Some(user)) = (attr("data-admin-act"), attr("data-admin-user")) {
        let Some(act) = AdminAct::from_name(&name) else {
            return true;
        };
        dispatch(Action::AdminActed { user, act });
    } else if let Some(name) = attr("data-act") {
        match name.as_str() {
            "sign-out" => dispatch(Action::SignOutRequested),
            "enroll-passkey" => dispatch(Action::EnrollPasskeyRequested),
            "passkey-sign-in" => dispatch(Action::PasskeySignInRequested {
                username: field_value("username"),
            }),
            "admin-toggle" | "dismiss-admin" => dispatch(Action::AdminToggled),
            "dismiss-palette" => dispatch(Action::PaletteDismissed),
            "new-project" => dispatch(Action::NewProjectRequested),
            _ => {}
        }
    } else {
        return false;
    }
    true
}

/// The two forms in the room: the sign-in card and a chat's composer.
fn submitted(form: &web_sys::Element) {
    if form.id() == "sign-in" {
        dispatch(Action::SignInSubmitted {
            username: field_value("username"),
            code: field_value("code"),
        });
    } else if let Some(id) = form.get_attribute("data-chat") {
        let field = format!("composer-{id}");
        let text = field_value(&field);
        clear_field(&field);
        dispatch(Action::ChatPosted { id, text });
    }
}

/// The current text of a named field, or nothing if it is not there.
fn field_value(id: &str) -> String {
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    else {
        return String::new();
    };
    if let Some(input) = el.dyn_ref::<web_sys::HtmlInputElement>() {
        input.value()
    } else if let Some(area) = el.dyn_ref::<web_sys::HtmlTextAreaElement>() {
        area.value()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// The workspace: sidebar tree + (for now) an empty stage
// ---------------------------------------------------------------------------

/// The sidebar's contents: the tree, the create row, and who you are.
fn sidebar_view(state: &State, user: &crate::core::User) -> String {
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

    format!(
        r#"<div class="shell-brand"><span class="spore">●</span> myco</div>
           {chip}
           <div class="tree">{tree}</div>
           <div class="row-buttons creates">{creates}</div>
           <div class="sidebar-foot">
             <span class="dim">{user}</span>
             <div class="row-buttons">{admin_button}
               <button data-act="enroll-passkey" class="quiet-button">add a passkey</button>
               <button data-act="sign-out" class="quiet-button">sign out</button>
             </div>
           </div>
           {note}
           <div class="status-line">{feed}</div>"#,
        user = escape(user.display()),
        admin_button = if state.admin.is_some() {
            r#"<button data-act="admin-toggle" class="quiet-button">admin</button>"#
        } else {
            ""
        },
        note = match &state.passkey_note {
            Some(note) => format!(r#"<div class="dim passkey-note">{}</div>"#, escape(note)),
            None => String::new(),
        },
    )
}

/// The stage's contents: every open pane, or the invitation.
fn stage_view(state: &State) -> String {
    let ws = &state.workspace;
    if ws.panes.is_empty() {
        r#"<div class="dim stage-empty">open an instance from the tree</div>"#.to_string()
    } else {
        ws.panes
            .iter()
            .map(|p| pane_view(p, ws, state.renaming.as_ref()))
            .collect()
    }
}

/// The last palette answer, quiet at the bottom of the room. The palette
/// covering it wins; two floating things at once is noise.
fn notice_view(state: &State) -> String {
    match &state.notice {
        Some(notice) if state.palette.is_none() => format!(
            r#"<div class="island notice mono">{}</div>"#,
            escape(notice)
        ),
        _ => String::new(),
    }
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
        Some(view) if pane.kind == "chat" => chat_transcript(view, pane),
        Some(view) if pane.kind == "host" => host_card(view),
        Some(view) => format!(
            r#"<pre class="mono pane-body">{}</pre>"#,
            escape(&pretty(view))
        ),
        None => r#"<div class="dim">reading…</div>"#.to_string(),
    };
    let focused = ws.selected.as_deref() == Some(pane.id.as_str());
    format!(
        r#"<div class="island pane{gone}{focus}" data-focus="{id}">
             <div class="pane-header">
               {title_node}
               <span class="pane-chrome">{chip}{release}
                 <button class="quiet-button" data-close="{id}">close</button></span>
             </div>
             {body}{view}
           </div>"#,
        gone = if pane.gone { " pane-gone" } else { "" },
        focus = if focused { " focused" } else { "" },
        title_node = title_node(&pane.id, &title, renaming, true),
        id = escape(&pane.id),
    )
}

/// The chat renderer: the `tail` payload as a transcript — bylines with
/// the presence dot, streaming turns still running shown live (an
/// assistant entry with no turn_end is a breathing thought, not history),
/// tool calls and their results folded quietly, watched splices set
/// apart. Below it, the composer.
fn chat_transcript(raw: &str, pane: &crate::core::Pane) -> String {
    #[derive(serde::Deserialize)]
    struct Author {
        kind: String,
        id: String,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        author: Author,
        #[serde(rename = "t")]
        t: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        content: Vec<serde_json::Value>,
        #[serde(default)]
        tool_uses: Vec<serde_json::Value>,
        #[serde(default)]
        results: Vec<serde_json::Value>,
        #[serde(default)]
        instance: String,
        #[serde(default)]
        data: String,
        #[serde(default)]
        turn_end: Option<serde_json::Value>,
    }
    #[derive(serde::Deserialize)]
    struct Tail {
        #[serde(default)]
        entries: Vec<Entry>,
    }
    let Ok(tail) = serde_json::from_str::<Tail>(raw) else {
        return format!(
            r#"<pre class="mono pane-body">{}</pre>"#,
            escape(&pretty(raw))
        );
    };
    let mut running = false;
    let body: String = tail
        .entries
        .iter()
        .map(|e| {
            let dot = match e.author.kind.as_str() {
                "human" => "human",
                "agent" => "agent",
                _ => "system",
            };
            match e.t.as_str() {
                "message" => bubble(dot, &e.author.id, &escape(&e.text), ""),
                "assistant" => {
                    let text = content_text(&e.content);
                    let streaming = e.turn_end.is_none();
                    running |= streaming;
                    let tools: String = e
                        .tool_uses
                        .iter()
                        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                        .map(|name| {
                            format!(r#"<span class="tool-chip mono">⚙ {}</span>"#, escape(name))
                        })
                        .collect();
                    let cursor = if streaming {
                        r#"<span class="stream-cursor"></span>"#
                    } else {
                        ""
                    };
                    bubble(dot, "agent", &format!("{}{cursor}", escape(&text)), &tools)
                }
                "tool_results" => {
                    let text = e
                        .results
                        .iter()
                        .filter_map(|r| r.get("content").and_then(content_of))
                        .collect::<Vec<_>>()
                        .join(
                            "
",
                        );
                    format!(
                        r#"<details class="tool-result"><summary class="dim">tool result</summary>
                             <pre class="mono">{}</pre></details>"#,
                        escape(&text)
                    )
                }
                "watched" => format!(
                    r#"<div class="watched dim"><span class="mono">watched {}</span>
                         <pre class="mono">{}</pre></div>"#,
                    escape(&e.instance),
                    escape(&e.data),
                ),
                _ => String::new(),
            }
        })
        .collect();

    let modeled = pane_is_modeled(pane);
    let composer = if modeled {
        let cancel = if running {
            format!(
                r#"<button class="quiet-button" data-cancel="{id}">cancel turn</button>"#,
                id = escape(&pane.id)
            )
        } else {
            String::new()
        };
        format!(
            r#"<form class="composer" data-chat="{id}">
                 <textarea id="composer-{id}" class="composer-input" rows="1"
                   placeholder="message — enter to send, shift-enter for a newline">{draft}</textarea>
                 <div class="composer-foot">{cancel}
                   <button class="primary-button">send</button></div>
               </form>"#,
            id = escape(&pane.id),
            draft = escape(&pane.draft),
        )
    } else {
        r#"<div class="dim composer-note">this chat has no model — it is a shared transcript.
             post with the palette or the API.</div>"#
            .to_string()
    };

    format!(r#"<div class="pane-body chat"><div class="transcript">{body}</div>{composer}</div>"#)
}

/// Whether the chat pane's instance carries a model driver — a modeled
/// chat is driven by its own agent, which the listing shows as the seat.
fn pane_is_modeled(pane: &crate::core::Pane) -> bool {
    // The chat's `about` isn't in the pane; infer from the driver being an
    // agent named for this instance (the doctrine's naming). Absent that,
    // still offer the composer — worst case a post to a modelless chat
    // simply appends without a reply.
    let _ = pane;
    true
}

fn bubble(dot: &str, who: &str, text: &str, extra: &str) -> String {
    format!(
        r#"<div class="entry">
             <div class="byline"><span class="seat {dot}"></span>
               <span class="dim">{who}</span></div>
             <div class="entry-body">{text}{extra}</div>
           </div>"#,
        who = escape(who),
    )
}

/// Concatenate the text blocks of a content run (the wire's serde tags).
fn content_text(content: &[serde_json::Value]) -> String {
    content
        .iter()
        .filter_map(|c| {
            c.get("Text")
                .and_then(|t| t.get("text"))
                .and_then(|t| t.as_str())
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        )
}

fn content_of(content: &serde_json::Value) -> Option<String> {
    content.as_array().map(|blocks| {
        blocks
            .iter()
            .filter_map(|b| {
                b.get("Text")
                    .and_then(|t| t.get("text"))
                    .and_then(|t| t.as_str())
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    })
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

/// How deep the tree will indent before it gives up. Parentage is acyclic
/// by construction at L1, so this can only ever fire on a server the client
/// should not have trusted — and a bounded lie renders better than a hang.
const MAX_TREE_DEPTH: usize = 8;

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
                     <button data-act="new-project" class="quiet-button">+ project</button>
                   </div>"#,
                value = escape(&ws.current_project),
                label = escape(label),
            )
        }
    }
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

/// A host pane: the connection's status (its `about`, kept live by the
/// ordinary watch), what the far side offers, and where the actions live
/// — the palette, because host verbs are palette rows like any others.
fn host_card(view: &str) -> String {
    let about: serde_json::Value = serde_json::from_str(view).unwrap_or_default();
    let status = about["status"].as_str().unwrap_or("dialing");
    let tone = match status {
        "up" => "agent",
        "dialing" => "human",
        _ => "system",
    };
    let name = about["name"].as_str().unwrap_or("(not yet announced)");
    let command = about["command"].as_str().unwrap_or("");
    let detail = about["detail"].as_str().unwrap_or("");
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="form-error">{}</div>"#, escape(detail))
    };
    let offers = about["kinds"]
        .as_array()
        .map(|kinds| {
            kinds
                .iter()
                .filter_map(|k| {
                    Some(format!(
                        "{} v{}",
                        k["kind"].as_str()?,
                        k["version"].as_u64().unwrap_or(0)
                    ))
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default();
    let offers = if offers.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div class="dim">offers <span class="mono">{}</span></div>"#,
            escape(&offers)
        )
    };
    format!(
        r#"<div class="host-card">
             <div class="host-status"><span class="seat {tone}"></span>
               {status} as <b>{name}</b></div>
             <div class="dim mono">{command}</div>
             {detail}
             {offers}
             <div class="dim">create over there with the host's
               <span class="mono">new</span> verb — it is in the palette</div>
           </div>"#,
        status = escape(status),
        name = escape(name),
        command = escape(command),
    )
}

/// The operator's panel: the roster as rows with the v2 panel's actions,
/// the freshly minted code shown large (its only plaintext moment), and
/// refusals in the server's words. Summoned like the palette; forgets the
/// code when closed.
fn admin_overlay(state: &State) -> String {
    let Some(admin) = &state.admin else {
        return String::new();
    };
    if !admin.open {
        return String::new();
    }
    let minted = match &admin.minted {
        Some(minted) => format!(
            r#"<div class="minted island">
                 <div class="dim">one-time code for <b>{user}</b> — single use, it will not
                   be shown again</div>
                 <div class="minted-code mono">{code}</div>
               </div>"#,
            user = escape(&minted.username),
            code = escape(&minted.code),
        ),
        None => String::new(),
    };
    let error = match &admin.error {
        Some(why) => format!(r#"<div class="form-error">{}</div>"#, escape(why)),
        None => String::new(),
    };
    let rows: String = admin
        .users
        .iter()
        .map(|user| {
            let flags = format!(
                "{}{} · {} session{} · {} passkey{}",
                if user.operator { "operator · " } else { "" },
                if user.disabled { "disabled" } else { "active" },
                user.sessions,
                if user.sessions == 1 { "" } else { "s" },
                user.passkeys,
                if user.passkeys == 1 { "" } else { "s" },
            );
            let act = |name: &str, label: &str| {
                format!(
                    r#"<button class="quiet-button" data-admin-act="{name}"
                        data-admin-user="{id}">{label}</button>"#,
                    id = escape(&user.id),
                )
            };
            let mut actions = act("mint", "mint code");
            if !user.operator {
                actions += &if user.disabled {
                    act("enable", "enable")
                } else {
                    act("disable", "disable")
                };
            }
            actions += &act("revoke", "revoke");
            actions += &act("clear-passkeys", "clear passkeys");
            if !user.operator {
                actions += &act("remove", "remove");
            }
            format!(
                r#"<div class="admin-row">
                     <div><b>{id}</b> <span class="dim">{name}</span>
                       <div class="dim admin-flags">{flags}</div></div>
                     <div class="row-buttons admin-actions">{actions}</div>
                   </div>"#,
                id = escape(&user.id),
                name = escape(&user.name),
                flags = escape(&flags),
            )
        })
        .collect();
    format!(
        r#"<div class="palette-scrim" data-act="dismiss-admin">
             <div class="island palette admin-panel" data-keep>
               <div class="palette-group">the roster</div>
               {error}{minted}{rows}
             </div>
           </div>"#
    )
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
            target,
            draft,
            error,
        } => {
            let error = match error {
                Some(why) => format!(r#"<div class="form-error">{}</div>"#, escape(why)),
                None => String::new(),
            };
            format!(
                r#"<div class="dim"><span class="mono">{label}</span> wants arguments — JSON,
                     enter to run, esc to go back</div>
                   {error}
                   <textarea id="palette-args" class="mono" rows="4">{draft}</textarea>"#,
                label = escape(&target.label()),
                draft = escape(draft),
            )
        }
    };
    format!(
        r#"<div class="palette-scrim" data-act="dismiss-palette">
             <div class="island palette" id="palette" data-keep>{inner}</div>
           </div>"#
    )
}

/// Measure the focused tty pane's cell grid and offer it to the reducer,
/// which resizes at most once per distinct size — so render → measure →
/// dispatch cannot spin (the repeat measurement produces no effect and no
/// state change worth re-rendering... except the action log; hence the
/// edge-side dedupe here too).
fn measure_focused_tty(document: &web_sys::Document) {
    thread_local! {
        static LAST: RefCell<Option<(String, u16, u16)>> = const { RefCell::new(None) };
    }
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
    // One character cell, measured off the live font: a hidden probe span.
    let Some((cw, ch)) = char_cell(document) else {
        return;
    };
    let cols = ((el.client_width() as f64 - 8.0) / cw)
        .floor()
        .clamp(20.0, 500.0) as u16;
    let rows = ((el.client_height() as f64) / ch).floor().clamp(5.0, 200.0) as u16;
    let id = STATE
        .with(|s| s.borrow().workspace.selected.clone())
        .unwrap_or_default();
    let fresh = LAST.with(|last| {
        let mut last = last.borrow_mut();
        if *last == Some((id.clone(), cols, rows)) {
            false
        } else {
            *last = Some((id.clone(), cols, rows));
            true
        }
    });
    if fresh && !id.is_empty() {
        dispatch(Action::PaneMeasured { id, cols, rows });
    }
}

fn char_cell(document: &web_sys::Document) -> Option<(f64, f64)> {
    let probe = document.create_element("span").ok()?;
    probe.set_class_name("mono tty-probe");
    probe.set_text_content(Some("MMMMMMMMMM"));
    document.body()?.append_child(&probe).ok()?;
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
