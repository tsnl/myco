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

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("myco: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("myco: serving http://{addr}/api");
    if let Err(e) = axum::serve(listener, myco_server::router(pool)).await {
        eprintln!("myco: server error: {e}");
        std::process::exit(1);
    }
}
