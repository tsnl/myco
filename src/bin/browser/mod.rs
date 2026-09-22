//! HTTPS browser frontend. Tabs observe independently owned session workers.

use std::sync::Arc;

use super::Args;

mod assets;
mod auth;
mod files;
mod http;
mod markdown;
mod runtime;
mod tls;
mod view;
mod weather;

/// A URL a browser can open for a listener bound to `address`.
///
/// A wildcard bind has no address to dial, so use this machine's hostname and
/// let the reader's resolver find a route; the served origin is whatever `Host`
/// they arrive with, not this string.
fn launch_origin(address: std::net::SocketAddr, secure: bool) -> String {
    let scheme = if secure { "https" } else { "http" };
    if address.ip().is_unspecified() {
        return format!("{scheme}://{}:{}", hostname(), address.port());
    }
    format!("{scheme}://{address}")
}

fn hostname() -> String {
    let mut buffer = [0 as libc::c_char; 256];
    // SAFETY: libc writes at most `buffer.len()` bytes into our own buffer.
    let name = (unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len()) } == 0)
        .then(|| {
            // A truncated name is not NUL-terminated on every platform, so stop
            // at the last byte rather than trusting the terminator.
            let bytes = buffer.map(|c| c as u8);
            let end = bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(bytes.len() - 1);
            String::from_utf8(bytes[..end].to_vec()).ok()
        })
        .flatten();
    name.filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

pub(super) async fn run(args: Args) -> Result<(), String> {
    let tls = tls::configure(&args, &hostname()).await?;
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
        launch_origin(address, tls.is_some()),
        address.port(),
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
    if !address.ip().is_loopback() {
        println!(
            "Listening on {address} — anyone who can route here and holds that URL reaches these sessions."
        );
    }
    println!("Press Ctrl-C here to stop the server. Browser tabs keep independent sessions alive.");
    let handle = axum_server::Handle::new();
    let listener = listener.into_std().map_err(|e| e.to_string())?;
    let app = http::router(server.clone()).into_make_service();
    let serving = async {
        match tls {
            Some(tls) => {
                axum_server::from_tcp_rustls(listener, tls)?
                    .handle(handle.clone())
                    .serve(app)
                    .await
            }
            None => {
                axum_server::from_tcp(listener)?
                    .handle(handle.clone())
                    .serve(app)
                    .await
            }
        }
    };
    tokio::pin!(serving);
    let result = tokio::select! {
        result = &mut serving => result,
        _ = tokio::signal::ctrl_c() => {
            server.sessions.stop().await;
            handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
            serving.await
        }
    }
    .map_err(|e| e.to_string());
    server.sessions.stop().await;
    server.sessions.join().await.map_err(|e| e.to_string())?;
    result
}
