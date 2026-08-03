//! The `myco` binary. Default mode is the interactive CLI (async: input
//! queues while turns run). `--mode serve` runs the multiplayer web server —
//! the parallel experiment — over the same session runtime. `--mode host` is
//! the worker remotes run (`ssh <alias> myco --mode host`).

use clap::{Parser, ValueEnum};
use myco::config::{Config, ConfigUserSettings, DEFAULT_MAX_IMAGE_BASE64_BYTES};
use myco::host::HostWorker;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Interactive CLI (default).
    Cli,
    /// Serve the web API + GUI on localhost.
    Serve,
    /// Serve OS tools over stdin/stdout NDJSON (spawned as `ssh <alias> myco
    /// --mode host`; never run by hand).
    Host,
}

#[derive(Parser, Debug)]
#[command(name = "myco", version, about = "myco: multi-host coding agent")]
struct Args {
    #[arg(long, value_enum, default_value_t = Mode::Cli)]
    mode: Mode,

    /// Print mode: run one agent turn, print the answer to stdout, exit.
    /// The session is saved like any other (`session=<id>` on stderr).
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<String>,

    /// Resume a session by id (or prefix). Without a value: the most recent.
    #[arg(long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    /// Create this session as a hidden child of the given session (nested
    /// agents; combine with --fork to copy the parent's conversation).
    #[arg(long, value_name = "ID")]
    parent_session: Option<String>,

    /// With --parent-session: seed the child with the parent's saved
    /// conversation (context fork).
    #[arg(long)]
    fork: bool,

    /// Server port for `--mode serve`.
    #[arg(long, default_value_t = 7773)]
    port: u16,

    /// Config file override (`$MYCO_CONFIG` → `~/.myco/config.toml`).
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

    /// User roster override (`$MYCO_SERVER_CONFIG` → `~/.myco/v2/server.toml`).
    #[arg(long, value_name = "PATH")]
    server_config: Option<std::path::PathBuf>,

    /// Model key from the config.toml [models] catalog.
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
        Mode::Serve => {
            let config = match Config::resolve(ConfigUserSettings {
                config_path: args.config,
                roster_path: args.server_config,
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
            if let Err(e) = myco::web::serve(config, args.port).await {
                eprintln!("myco server error: {e}");
                std::process::exit(1);
            }
        }
        Mode::Cli => {
            myco::cli::run(myco::cli::CliOptions {
                config_path: args.config,
                roster_path: args.server_config,
                model: args.model,
                resume: args
                    .resume
                    .map(|id| if id.is_empty() { None } else { Some(id) }),
                parent_session: args.parent_session,
                fork: args.fork,
                print: args.print,
            })
            .await;
        }
    }
}
