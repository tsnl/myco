//! The `myco` binary. Default mode is the interactive CLI (async: input
//! queues while turns run). `--mode serve` runs the multiplayer web server —
//! the parallel experiment — over the same session runtime. `--mode host` is
//! the worker remotes run (`ssh <alias> myco --mode host`).

use clap::{Parser, Subcommand, ValueEnum};
use myco::config::{Config, ConfigUserSettings, DEFAULT_MAX_IMAGE_BASE64_BYTES};
use myco::host::HostWorker;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Thin client (default): `-p` runs one turn against the local server,
    /// spawning `--mode serve` if none answers.
    Cli,
    /// Serve the web API + GUI on localhost.
    Serve,
    /// Serve OS tools over stdin/stdout NDJSON (spawned as `ssh <alias> myco
    /// --mode host`; never run by hand).
    Host,
}

/// Subcommands that do administration rather than run an agent.
#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Manage who can sign in to the web server.
    Auth {
        #[command(subcommand)]
        command: myco::admin::AuthCommand,
    },
}

#[derive(Parser, Debug)]
#[command(name = "myco", version, about = "myco: multi-host coding agent")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long, value_enum, default_value_t = Mode::Cli)]
    mode: Mode,

    /// Print mode: run one agent turn over the API, print the answer to
    /// stdout, exit. The session is saved like any other (`session=<id>` on
    /// stderr).
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<String>,

    /// With -p: continue this session (id or prefix) instead of creating one.
    #[arg(long, value_name = "ID")]
    session: Option<String>,

    /// Server port (`--mode serve` listens here; `-p` connects here).
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

    // Administration runs and exits; it never starts a runtime.
    if let Some(Command::Auth { command }) = args.command {
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
        std::process::exit(myco::admin::run(&config, command));
    }

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
            let Some(prompt) = args.print else {
                eprintln!(
                    "myco is a server with clients, not a terminal REPL.\n\
                     - `myco --mode serve` then open the web GUI (`trunk serve`, :8080)\n\
                     - `myco -p \"prompt\"` for a one-shot turn (spawns the server if needed)\n\
                     - `clients/myco.py` or plain HTTP for scripts (see README, API section)"
                );
                std::process::exit(2);
            };
            let code = myco::cli::run_print(myco::cli::PrintOptions {
                prompt,
                model: args.model,
                session: args.session,
                port: args.port,
            })
            .await;
            std::process::exit(code);
        }
    }
}
