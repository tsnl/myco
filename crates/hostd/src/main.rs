//! `myco-hostd` — a host, which is nothing but the provider serve loop
//! with `kind-tty` registered: the same L0/L1 the server runs, in a
//! small static binary a server reaches over ssh (`ssh box myco-hostd`).
//! stdout is the protocol; everything human goes to stderr.

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "myco-hostd",
    about = "serve this machine's kinds to a myco server over stdio"
)]
struct Args {
    /// The name this host announces (defaults to the machine's hostname).
    #[arg(long)]
    name: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let name = args.name.unwrap_or_else(hostname);

    let pool = myco_instance::Pool::new();
    pool.register(Arc::new(myco_kind_tty::TtyKind));

    eprintln!("myco-hostd: serving as {name:?} on stdio");
    match myco_provider::serve(pool, &name, tokio::io::stdin(), tokio::io::stdout()).await {
        Ok(()) => {
            eprintln!("myco-hostd: the pool hung up");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("myco-hostd: {e}");
            ExitCode::FAILURE
        }
    }
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    if ok {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        if let Ok(name) = std::str::from_utf8(&buf[..end])
            && !name.is_empty()
        {
            return name.to_string();
        }
    }
    "unnamed-host".into()
}
