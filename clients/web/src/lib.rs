//! The myco web client — Rust compiled to wasm, driving the DOM directly.
//! The stack decision this crate records (DESIGN.md L3): no TypeScript, no
//! JS framework; the reducer core that arrives in the next PR is plain
//! Rust, so a native client (DP‑1) reuses everything but the render layer.
//!
//! This PR is the scaffold: the amethyst shell (STYLE.md tokens, both
//! themes), served from the `myco` binary at `/` — one origin, no proxy —
//! and a liveness probe against `/api` so the very first pixel already
//! tells the truth about the server.

#![cfg(target_arch = "wasm32")]

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {
    let document = web_sys::window()
        .expect("a window")
        .document()
        .expect("a document");
    let body = document.body().expect("a body");
    body.set_inner_html(
        r#"<div class="shell-center">
             <div class="island shell-card">
               <div class="shell-brand"><span class="spore">●</span> myco</div>
               <div class="dim">a workspace of instances, shared by humans and agents</div>
               <div class="status-line">
                 <span class="status-dot" id="dot"></span>
                 <span id="status">reaching the server…</span>
               </div>
               <div class="dim mono">sign-in arrives with the next stack PR</div>
             </div>
           </div>"#,
    );
    wasm_bindgen_futures::spawn_local(probe(document));
}

/// One fetch against `/api/whoami`. A 401 is the healthy answer — the
/// server is there and refusing anonymity, exactly as built. The lowercase
/// voice starts on the first screen.
async fn probe(document: web_sys::Document) {
    let (class, message) = match fetch_status("/api/whoami").await {
        Some(401) => ("ok", "server reachable — not signed in"),
        Some(200) => ("ok", "server reachable — a token is already valid"),
        Some(code) => {
            let _ = code;
            ("attn", "server answered strangely — check the logs")
        }
        None => ("attn", "no server — is `myco` running on this origin?"),
    };
    if let Some(dot) = document.get_element_by_id("dot") {
        dot.set_class_name(&format!("status-dot {class}"));
    }
    if let Some(status) = document.get_element_by_id("status") {
        status.set_text_content(Some(message));
    }
}

async fn fetch_status(url: &str) -> Option<u16> {
    let window = web_sys::window()?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url))
        .await
        .ok()?;
    let response: web_sys::Response = response.dyn_into().ok()?;
    Some(response.status())
}
