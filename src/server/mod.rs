//! `myco --mode server`: an HTTP frontend over the same harness, sessions, and
//! hosts as the CLI.
//!
//! - `/api/**` — REST (JSON) + a Server-Sent Events feed per session.
//! - `/` and other paths — the built Trunk GUI (production), when `dist_dir` is
//!   set. In development you run `trunk serve` (hot-reload) which serves the
//!   client and reverse-proxies `/api` back to this server (see `Trunk.toml`).
//!
//! The server reuses `~/.myco/{config,session}` exactly like `--mode interactive`;
//! it is a second front-end, not a second runtime. Session lifecycle and model
//! construction live in [`crate::repl`]; this module is the HTTP adapter.

mod routes;
mod state;
mod wire;

use std::path::PathBuf;

use crate::repl::Repl;

pub use state::AppState;
pub use wire::{WireContent, WireEvent};

/// Configuration for the HTTP server (assembled from CLI flags in `main`).
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    /// Directory of built GUI assets to serve at `/`. `None` → API only (dev
    /// mode: run Trunk for the client + hot-reload).
    pub dist_dir: Option<PathBuf>,
}

/// Build and launch the rocket server. Blocks until shutdown.
pub async fn serve(repl: Repl, config: ServerConfig) -> Result<(), String> {
    let ServerConfig {
        bind,
        port,
        dist_dir,
    } = config;

    let state = AppState::new(repl, dist_dir);

    let figment = rocket::Config::figment()
        .merge(("address", bind))
        .merge(("port", port))
        // Long-lived SSE connections must not be reaped by keep-alive.
        .merge(("keep_alive", 0u32));

    let _rocket = rocket::custom(figment)
        .manage(state)
        .mount(
            "/",
            rocket::routes![
                routes::health,
                routes::hosts,
                routes::list_sessions,
                routes::create_session,
                routes::get_session,
                routes::patch_session,
                routes::send_message,
                routes::cancel_session,
                routes::session_events,
                routes::spa,
            ],
        )
        .launch()
        .await
        .map_err(|e| format!("server error: {e}"))?;

    Ok(())
}
