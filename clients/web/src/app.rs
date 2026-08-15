//! The wasm edge: dispatch, render, effects. An `onClick` is a dispatch;
//! an effect's completion is a dispatch; nothing here holds state or
//! contains logic — the reducer is the whole brain.

use std::cell::RefCell;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::core::{Action, Effect, Session, State, reduce};

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[wasm_bindgen(start)]
pub fn start() {
    dispatch(Action::Boot);
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
        Effect::Whoami => wasm_bindgen_futures::spawn_local(async {
            dispatch(match fetch_text("/api/whoami").await {
                Ok((status, body)) => Action::WhoamiAnswered { status, body },
                Err(what) => Action::NetworkFailed { what },
            });
        }),
    }
}

async fn fetch_text(url: &str) -> Result<(u16, String), String> {
    let window = web_sys::window().ok_or("no window")?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|_| format!("fetch {url}"))?;
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

/// Render is a pure function of state. At this stack position the whole
/// UI is one island; pane-grained rendering arrives with the workspace.
fn render(state: &State) {
    let (dot, line) = match &state.session {
        Session::Checking => ("", "reaching the server…".to_string()),
        Session::SignedOut => ("ok", "server reachable — not signed in".to_string()),
        Session::SignedIn(user) => {
            let name = if user.name.is_empty() {
                &user.id
            } else {
                &user.name
            };
            ("ok", format!("signed in as {name}"))
        }
        Session::Unreachable { why } => ("attn", why.clone()),
    };
    let html = format!(
        r#"<div class="shell-center">
             <div class="island shell-card">
               <div class="shell-brand"><span class="spore">●</span> myco</div>
               <div class="dim">a workspace of instances, shared by humans and agents</div>
               <div class="status-line">
                 <span class="status-dot {dot}"></span>
                 <span>{line}</span>
               </div>
               <div class="dim mono">sign-in arrives with the next stack PR</div>
             </div>
           </div>"#,
        dot = dot,
        line = escape(&line),
    );
    if let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) {
        body.set_inner_html(&html);
    }
}

/// Text into markup, safely. Server-controlled strings still get escaped —
/// the render layer trusts nothing it did not write.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
