//! Loopback browser frontend. Tabs observe independently owned session workers.

use std::sync::Arc;

use super::Args;

mod http;
mod markdown;
mod runtime;
mod view;

pub(super) async fn run(args: Args) -> Result<(), String> {
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, args.web.unwrap()))
            .await
            .map_err(|e| format!("cannot listen for browser UI: {e}"))?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let (config, catalog, preflight) = super::prepare_boot(&args);
    let initial = (args.resume.is_some() || args.parent_session.is_some())
        .then(|| super::initial_session_or_exit(&args, &catalog.spec.key));
    let launch_path = initial
        .as_ref()
        .map_or_else(|| "/".into(), |s| format!("/sessions/{}", s.id));
    let server = Arc::new(http::Server::new(
        runtime::Sessions::new(args, config, preflight),
        address,
        launch_path,
    ));
    if let Some(session) = initial {
        server
            .sessions
            .start(session)
            .await
            .map_err(|e| e.to_string())?;
    }
    println!("Browser UI: {}/auth?token={}", server.origin, server.token);
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
