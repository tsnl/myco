//! Loopback browser frontend. Tabs observe independently owned session workers.

use std::sync::Arc;

use super::Args;

mod assets;
mod auth;
mod files;
mod http;
mod markdown;
mod runtime;
mod view;
mod weather;

pub(super) async fn run(args: Args) -> Result<(), String> {
    let files = files::Files::open(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let listener = tokio::net::TcpListener::bind((args.bind, args.port))
        .await
        .map_err(|e| format!("cannot listen for browser UI: {e}"))?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let (config, _, preflight) = super::prepare_boot(&args);
    let initial = args
        .resume
        .as_deref()
        .map(myco::Session::load_by_id_or_prefix)
        .transpose()?;
    let launch_path = initial
        .as_ref()
        .map_or_else(|| "/".into(), |s| format!("/sessions/{}", s.id));
    let server = Arc::new(http::Server::new(
        runtime::Sessions::new(args, config, preflight),
        format!("http://{address}"),
        launch_path,
        files,
    ));
    if let Some(session) = initial {
        server
            .sessions
            .start(session)
            .await
            .map_err(|e| e.to_string())?;
    }
    println!(
        "Browser UI: {}/auth?token={}",
        server.auth.origin, server.auth.token
    );
    println!(
        "Listening on loopback only. Use an SSH tunnel for remote access (myco --help browser)."
    );
    println!("Press Ctrl-C here to stop the server. Browser tabs keep independent sessions alive.");
    let shutdown = server.clone();
    let result = axum::serve(listener, http::router(server.clone()))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.sessions.stop().await;
        })
        .await
        .map_err(|e| e.to_string());
    server.sessions.stop().await;
    server.sessions.join().await.map_err(|e| e.to_string())?;
    result
}
