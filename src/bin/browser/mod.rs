//! Loopback browser frontend. Tabs observe independently owned session workers.

use std::sync::Arc;

use super::Args;

mod assets;
mod attachments;
mod files;
mod http;
mod markdown;
mod origin;
mod profile_worker;
mod profiles;
mod runtime;
mod view;
mod weather;

pub(super) async fn run(args: Args) -> Result<(), String> {
    if args.profile_worker.is_none() {
        return profiles::run(args).await;
    }
    run_worker(args).await
}

async fn run_worker(args: Args) -> Result<(), String> {
    let listener = tokio::net::UnixListener::bind(
        args.profile_worker
            .as_ref()
            .ok_or("missing worker socket")?,
    )
    .map_err(|e| format!("cannot listen for profile: {e}"))?;
    let profile = std::env::var("MYCO_PROFILE").map_err(|e| e.to_string())?;
    let files = files::Files::open(&std::env::current_dir().map_err(|e| e.to_string())?)?
        .with_base_path(format!("/profiles/{profile}"));
    let getlink = Arc::new(myco::tool_services::GetLinkTool::new(
        files.workspace.clone(),
    ));
    let (config, _, preflight) = super::prepare_boot(&args);
    let initial = args
        .resume
        .as_deref()
        .map(myco::Session::load_by_id_or_prefix)
        .transpose()?;
    let launch_path = initial.as_ref().map_or_else(
        || format!("/profiles/{profile}/"),
        |s| format!("/profiles/{profile}/sessions/{}", s.id),
    );
    let server = Arc::new(
        http::Server::new(
            runtime::Sessions::new(args, config, preflight, vec![getlink]),
            files,
        )
        .with_profile(&profile),
    );
    if let Some(session) = initial {
        server
            .sessions
            .start(session)
            .await
            .map_err(|e| e.to_string())?;
    }
    println!(
        "{}",
        serde_json::to_string(&profile_worker::Ready { launch_path }).map_err(|e| e.to_string())?
    );
    let shutdown = server.clone();
    let result = axum::serve(listener, http::router(server.clone()))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = parent_closed() => {},
            }
            shutdown.sessions.stop().await;
        })
        .await
        .map_err(|e| e.to_string());
    server.sessions.stop().await;
    server.sessions.join().await.map_err(|e| e.to_string())?;
    result
}

async fn parent_closed() {
    use std::os::fd::AsFd;
    use tokio::io::AsyncReadExt;
    let pipe = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .and_then(tokio::net::unix::pipe::Receiver::from_owned_fd);
    let Ok(mut pipe) = pipe else {
        return;
    };
    let mut bytes = [0; 256];
    while let Ok(count) = pipe.read(&mut bytes).await {
        if count == 0 {
            return;
        }
    }
}
