//! The `myco` binary: resolve the roster, open the credential store, build
//! the pool, register the kinds, serve `/api` on loopback. Loopback only,
//! by doctrine — the supported remote pattern is an SSH tunnel (DESIGN.md,
//! DP-2).

use std::sync::Arc;

use clap::Parser;
use myco_instance::Pool;
use myco_server::auth::AuthStore;
use myco_server::roster;

#[derive(Parser, Debug)]
#[command(name = "myco", version, about = "myco: a workspace of instances")]
struct Args {
    /// Port to serve the API on (loopback only).
    #[arg(long, default_value_t = 7773)]
    port: u16,
}

fn fatal(message: impl std::fmt::Display) -> ! {
    eprintln!("myco: {message}");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Who exists (the roster) and what they hold (the store). The roster
    // declares names; reconciling here means adding a [[users]] entry is
    // enough to mint that person a sign-in code, with no second step.
    let env = |k: &str| std::env::var(k).ok();
    let roster = roster::resolve_roster_path(&env)
        .and_then(|path| roster::load_file_roster(&path).map(|file| (path, file)))
        .and_then(|(path, file)| roster::Roster::resolve(path, file, &env))
        .unwrap_or_else(|e| fatal(e));
    let auth = AuthStore::default_path()
        .map_err(|e| e.to_string())
        .and_then(|p| AuthStore::open(p).map_err(|e| e.to_string()))
        .map(Arc::new)
        .unwrap_or_else(|e| fatal(format!("cannot open the credential store: {e}")));
    for user in roster.users() {
        if auth.get(&user.id).is_none() {
            let _ = auth.add_user(&user.id, user.display_name());
        }
    }

    // The operator: the roster's local user. Their boot token goes to
    // operator.token (0600) so sibling processes on this machine can
    // authenticate; it dies with the server. The startup code is how a
    // person signs in before any durable credential exists.
    let operator = roster.local().id.clone();
    match auth.issue_for(&operator) {
        Some(issued) => {
            if let Err(e) = myco_server::write_operator_token(&issued.access_token) {
                eprintln!("myco: could not write operator.token: {e}");
            }
        }
        None => eprintln!("myco: no store entry for {operator:?}; operator.token not written"),
    }
    eprintln!("{}", myco_server::startup_code(&auth, &operator));

    let pool = Pool::new();
    pool.register(Arc::new(myco_kind_tty::TtyKind));

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => fatal(format!("cannot bind {addr}: {e}")),
    };
    eprintln!("myco: serving http://{addr}/api");
    if let Err(e) = axum::serve(listener, myco_server::router(pool, auth)).await {
        fatal(format!("server error: {e}"));
    }
}
