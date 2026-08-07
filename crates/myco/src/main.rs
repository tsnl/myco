//! The `myco` binary: build the pool, register the kinds, serve `/api` on
//! loopback. Loopback only, by doctrine — the supported remote pattern is an
//! SSH tunnel (DESIGN.md, DP-2).

use std::sync::Arc;

use clap::Parser;
use myco_instance::Pool;

#[derive(Parser, Debug)]
#[command(name = "myco", version, about = "myco: a workspace of instances")]
struct Args {
    /// Port to serve the API on (loopback only).
    #[arg(long, default_value_t = 7773)]
    port: u16,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let pool = Pool::new();
    pool.register(Arc::new(myco_kind_tty::TtyKind));

    // Interim identity until the roster + bearer auth land (stack PR 4):
    // every request acts as the OS user. Loopback binding is what makes
    // this safe in the meantime.
    let user = std::env::var("MYCO_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "local".into());
    let local = myco_instance::Principal::Human(user);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("myco: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("myco: serving http://{addr}/api");
    if let Err(e) = axum::serve(listener, myco_server::router(pool, local)).await {
        eprintln!("myco: server error: {e}");
        std::process::exit(1);
    }
}
