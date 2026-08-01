//! The `myco` binary: an API server by default (`myco` → Rocket on
//! localhost:7773 serving `/api` for myco-gui and scripts), plus the worker
//! mode remotes depend on (`ssh <alias> myco --mode host`).

use clap::{Parser, ValueEnum};
use myco::config::{Config, ConfigUserSettings, DEFAULT_MAX_IMAGE_BASE64_BYTES};
use myco::host::HostWorker;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Serve the API on localhost (default).
    Server,
    /// Serve OS tools over stdin/stdout NDJSON (spawned as `ssh <alias> myco
    /// --mode host`; never run by hand).
    Host,
}

#[derive(Parser, Debug)]
#[command(name = "myco", version, about = "myco API server / host worker")]
struct Args {
    #[arg(long, value_enum, default_value_t = Mode::Server)]
    mode: Mode,

    /// Server port for `--mode server`.
    #[arg(long, default_value_t = 7773)]
    port: u16,

    /// Config file override (`$MYCO_CONFIG` → `~/.myco/config.toml`).
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

    /// Default model key override (config file `model` otherwise).
    #[arg(long)]
    model: Option<String>,

    /// Host name advertised in hello_ok / logs. Only used with `--mode host`.
    #[arg(long, default_value = "local")]
    name: String,

    /// Only used with `--mode host`: the agent side passes its model's
    /// resolved per-image cap so every host enforces the same limit.
    #[arg(long, default_value_t = DEFAULT_MAX_IMAGE_BASE64_BYTES)]
    max_image_base64_bytes: u64,
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    match args.mode {
        Mode::Host => {
            if let Err(e) = HostWorker::standard(args.name, args.max_image_base64_bytes)
                .serve_stdio()
                .await
            {
                eprintln!("myco host error: {e}");
                std::process::exit(1);
            }
        }
        Mode::Server => {
            let config = match Config::resolve(ConfigUserSettings {
                config_path: args.config,
                model: args.model,
            }) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("myco: {e}");
                    std::process::exit(1);
                }
            };
            eprintln!(
                "myco server: http://127.0.0.1:{}/api (model: {})",
                args.port, config.model
            );
            if let Err(e) = myco::server::serve(config, args.port).await {
                eprintln!("myco server error: {e}");
                std::process::exit(1);
            }
        }
    }
}
