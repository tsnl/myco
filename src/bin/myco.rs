use std::{path::PathBuf, sync::Arc};

use clap::{CommandFactory, Parser, ValueEnum};
use myco::chat::{ModelCompactor, SessionRunner, WorkflowEvent, persist_session};
use myco::generative_model::{
    self, BackendConfig, CatalogModel, Content, Effort, GenerativeModelConfig,
};
use myco::host::HostWorker;
use myco::session::{ActiveSession, Session, SessionLockError, SessionWriteLock};
use myco::{
    Agent, Config, ConfigUserSettings, EventSink, Harness, ListRecentService, PreludeTool,
    SessionHistoryTool, SessionMetaTool, StartupPreflight, prompts,
};

#[path = "browser/mod.rs"]
mod browser;
#[path = "cli/mod.rs"]
mod cli;

const SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a helpful assistant running in an agentic harness with unfettered computer access.
"#;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Myco coding agent: browser, terminal, or one-shot prompts",
    disable_help_flag = true
)]
struct Args {
    /// Run one prompt and write completed responses to stdout. Bare -p reads stdin;
    /// with a prompt, piped stdin is prepended as context.
    #[arg(short = 'p', long = "print", value_name = "PROMPT", num_args = 0..=1, conflicts_with_all = ["mode", "port", "bind"])]
    print: Option<Option<String>>,
    /// Server port (0 chooses a free port).
    #[arg(long, alias = "web", default_value = "8765", num_args = 0..=1, default_missing_value = "8765")]
    port: u16,
    /// Loopback listen address. Use an SSH tunnel for remote access.
    #[arg(long, alias = "web-bind", default_value = "127.0.0.1", value_parser = parse_loopback_arg)]
    bind: std::net::IpAddr,
    /// Print launcher help, or an embedded manual article.
    #[arg(long = "help", short = 'h', value_name = "ARTICLE", num_args = 0..=1, default_missing_value = "")]
    help_topic: Option<String>,
    /// Start the browser server, scrolling terminal chat, or internal SSH host worker.
    #[arg(long, value_enum, default_value_t = Mode::Server)]
    mode: Mode,
    /// Initial server profile / CLI profile: overrides MYCO_PROFILE (default: default).
    #[arg(long, value_name = "NAME", value_parser = myco::core::validate_profile)]
    profile: Option<String>,
    /// Private profile-worker socket, owned by the browser supervisor.
    #[arg(long, hide = true, conflicts_with = "print")]
    profile_worker: Option<PathBuf>,
    /// Host worker name (only used with --mode host).
    #[arg(long, default_value = "local")]
    name: String,
    /// Host worker image cap, in base64 bytes (only used with --mode host).
    #[arg(long, default_value_t = myco::config::DEFAULT_MAX_IMAGE_BASE64_BYTES)]
    max_image_base64_bytes: u64,
    /// Default model key from the config.toml catalog.
    #[arg(long)]
    model: Option<String>,
    /// Dump provider request bodies to stderr.
    #[arg(long)]
    debug_dump_api_requests: bool,
    /// Resume a saved session (id or unique prefix).
    #[arg(long, value_name = "SESSION_ID")]
    resume: Option<String>,
    /// Reasoning effort (low|medium|high|max).
    #[arg(long, value_parser = parse_effort_arg, default_value = "high")]
    effort: Effort,
    /// Config path; otherwise MYCO_CONFIG or the selected profile's config.toml.
    #[arg(long)]
    config: Option<PathBuf>,
}

fn parse_effort_arg(s: &str) -> Result<Effort, String> {
    s.parse()
}

fn parse_loopback_arg(value: &str) -> Result<std::net::IpAddr, String> {
    let address = if value == "localhost" {
        "127.0.0.1"
    } else {
        value
    };
    address
        .parse::<std::net::IpAddr>()
        .ok()
        .filter(std::net::IpAddr::is_loopback)
        .ok_or_else(|| {
            "--bind requires a loopback address; use an SSH tunnel for remote access".into()
        })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    Server,
    #[value(alias = "interactive")]
    Cli,
    Host,
}

fn main() {
    let args = Args::parse();
    if args.profile_worker.is_none() {
        let _ = dotenvy::dotenv();
    }
    if let Some(topic) = args.help_topic.as_deref() {
        print_launcher_help(topic);
        return;
    }
    if let Err(error) = configure_profile(args.profile.as_deref()) {
        eprintln!("myco: {error}");
        std::process::exit(2);
    }
    if (args.mode == Mode::Cli || args.print.is_some() || args.profile_worker.is_some())
        && let Err(error) = myco::session::migrate_archived_sessions()
    {
        eprintln!("warning: could not organize archived sessions: {error}");
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create async runtime")
        .block_on(async {
            match args.mode {
                Mode::Server if args.print.is_some() => {
                    let code = cli::run_print(args).await;
                    if code != 0 {
                        std::process::exit(code.into());
                    }
                }
                Mode::Server => {
                    if let Err(error) = browser::run(args).await {
                        eprintln!("myco: {error}");
                        std::process::exit(1);
                    }
                }
                Mode::Cli => {
                    let code = cli::run_interactive(args).await;
                    if code != 0 {
                        std::process::exit(code.into());
                    }
                }
                Mode::Host => run_host(args).await,
            }
        });
}

fn configure_profile(profile: Option<&str>) -> Result<(), String> {
    let inherited = std::env::var("MYCO_PROFILE").ok();
    let profile =
        myco::core::validate_profile(profile.or(inherited.as_deref()).unwrap_or("default"))?;
    let root = std::env::var_os("MYCO_HOME")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".myco")))
        .ok_or("could not resolve home directory")?;
    let root = std::path::absolute(root).map_err(|e| format!("resolve MYCO_HOME: {e}"))?;
    // Startup is single-threaded, before Tokio or tools can read the environment.
    // Local bash children inherit both selectors, including across cwd changes.
    unsafe {
        if inherited.as_deref().unwrap_or("default") != profile {
            std::env::remove_var("MYCO_SERVER_URL");
        }
        std::env::set_var("MYCO_PROFILE", profile);
        std::env::set_var("MYCO_HOME", root);
    }
    Ok(())
}

/// `--mode host`: serve OS tools over stdin/stdout NDJSON. Used for **remote**
/// hosts (ssh … myco --mode host). The agent-side local host is in-process and
/// does not spawn this mode.
async fn run_host(args: Args) {
    if let Err(e) = HostWorker::standard(args.name, args.max_image_base64_bytes)
        .serve_stdio()
        .await
    {
        eprintln!("myco host error: {e}");
        std::process::exit(1);
    }
}

fn resolve_app_config_or_exit(args: &Args) -> (Config, CatalogModel) {
    let app_config = Config::resolve(ConfigUserSettings {
        config_path: args.config.clone(),
        model: args.model.clone(),
    })
    .unwrap_or_else(|e| {
        eprintln!("Failed to load config: {e}");
        std::process::exit(2);
    });
    let catalog_model = match app_config.models.get(&app_config.model) {
        Ok(m) => m.clone(),
        Err(e) => {
            eprintln!("{e}");
            eprintln!("config: {}", app_config.config_path.display());
            std::process::exit(2);
        }
    };
    (app_config, catalog_model)
}

async fn attach_harness(
    app_config: &Config,
    preflight: &StartupPreflight,
    root_services: Vec<Arc<dyn myco::ToolService>>,
) -> Result<Arc<Harness>, String> {
    Harness::attach_with_root_services(app_config.harness.clone(), root_services)
        .await
        .map_err(|e| {
            let mut message = format!("Failed to attach harness: {e}\nhint: remote hosts come from ~/.ssh/config Host aliases; local needs no binary spawn");
            if !preflight.executables.is_clean() {
                let names: Vec<&str> = preflight
                    .executables
                    .missing
                    .iter()
                    .map(|m| m.name)
                    .collect();
                message.push_str(&format!("\nhint: missing executables: {}", names.join(", ")));
            }
            if preflight.ssh.has_problems() {
                message.push_str(
                    "\nhint: ssh-agent preflight reported missing keys or an unreachable agent; \
                     try `ssh-add -l` and `ssh-add --apple-use-keychain <key>`"
                );
            }
            message.push_str(&format!("\nconfig: {}", app_config.config_path.display()));
            message
        })
}

/// Take the write lock for a session that is about to go live.
///
/// `Err` means another myco process holds it: both would rewrite the whole
/// document every turn, so the loser's turns vanish. `Unavailable` is not fatal
/// — a filesystem without `flock` must not stop myco from running — so it warns
/// and returns `None`, and the caller proceeds unlocked.
fn lock_session_or_report(session_id: &str) -> Result<Option<SessionWriteLock>, String> {
    match SessionWriteLock::acquire(session_id) {
        Ok(lock) => Ok(Some(lock)),
        Err(SessionLockError::Busy { path }) => Err(format!(
            "session {session_id} is already open in another myco process.\n\
             hint: use that window, start a fresh session \
             (lock: {})",
            path.display()
        )),
        Err(e @ SessionLockError::Unavailable(_)) => {
            eprintln!("warning: {e}; continuing without a single-writer guard");
            Ok(None)
        }
    }
}

fn session_warning(message: &str) {
    eprintln!("warning: {message}");
}

/// The runner and its owned session resources, held until the frontend exits.
struct Boot {
    app_config: Config,
    catalog_model: CatalogModel,
    preflight: StartupPreflight,
    session: ActiveSession,
    /// Single-writer guard on the session, held for as long as it is live.
    /// `None` when locking is unavailable on this filesystem.
    _session_lock: Option<SessionWriteLock>,
    harness: Arc<Harness>,
    runner: SessionRunner,
}

fn prepare_boot(args: &Args) -> (Config, CatalogModel, StartupPreflight) {
    let (app_config, catalog_model) = resolve_app_config_or_exit(args);

    // Check fatal configuration before creating sessions or attaching hosts.
    if let Some(fatal) = myco::harness::fatal_startup_check(app_config.max_prelude_bytes) {
        eprintln!("myco: {fatal}");
        std::process::exit(1);
    }
    let preflight = StartupPreflight::run(&app_config.harness.remote_hosts);
    (app_config, catalog_model, preflight)
}

async fn boot_session<S: EventSink + 'static>(
    args: &Args,
    app_config: Config,
    catalog_model: CatalogModel,
    preflight: StartupPreflight,
    mut loaded: Session,
    mut root_tools: Vec<Arc<dyn myco::ToolService>>,
    make_sink: impl FnOnce(&Config, &StartupPreflight, &ActiveSession) -> Arc<S>,
) -> Result<(Boot, Arc<S>), String> {
    let session_lock = lock_session_or_report(&loaded.id)?;
    // A previous writer may have finished between discovery and acquiring the lock.
    if loaded.json_path().exists() {
        loaded = Session::load(&loaded.json_path())?;
    }
    // Session handle first so `session_meta` can share it with the agent harness.
    let session = ActiveSession::new(loaded);
    let sink = make_sink(&app_config, &preflight, &session);

    let session_tool =
        Arc::new(SessionMetaTool::new(session.clone())) as Arc<dyn myco::ToolService>;
    let history_tool = Arc::new(SessionHistoryTool::new()) as Arc<dyn myco::ToolService>;
    let list_recent_tool = Arc::new(ListRecentService::new()) as Arc<dyn myco::ToolService>;
    let prelude_tool =
        Arc::new(PreludeTool::new(app_config.max_prelude_bytes)) as Arc<dyn myco::ToolService>;
    root_tools.extend([session_tool, history_tool, list_recent_tool, prelude_tool]);
    let harness = attach_harness(&app_config, &preflight, root_tools).await?;

    let (model, prelude) = build_model(
        &catalog_model,
        &harness,
        args.debug_dump_api_requests,
        args.effort,
    )?;
    let runtime = myco::SessionRuntime::new(harness.clone(), session.clone());
    runtime.set_max_image_base64_bytes(catalog_model.spec.max_image_base64_bytes);
    let mut agent = Agent::new(model.clone(), runtime.clone(), sink.clone());
    agent
        .set_before_generation_notice(Some(myco::session_runtime::prelude_change_notices(prelude)));
    agent.set_retry_policy(catalog_model.backend.retry_policy());
    agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
    agent.set_max_truncated_resumes(catalog_model.spec.max_truncated_resumes);
    let mut runner = SessionRunner::new(agent, runtime)
        .await
        .map_err(|error| format!("cannot bind session: {error}"))?;
    runner
        .set_model(
            model,
            myco::ModelInfo::from_spec(&catalog_model.spec, Some(args.effort)),
        )
        .await
        .map_err(|error| format!("cannot record runtime: {error}"))?;
    // A fork may be opened again before its first submission. Derive its need
    // for a child identity stamp from persisted context, not launch flags.
    let saved = session.snapshot();
    runner.set_forked(
        saved.parent_session_id.is_some()
            && !runner.agent().history().iter().any(|message| {
                matches!(message, myco::generative_model::Message::UserMessage { content }
            if content.iter().any(|part| matches!(part, Content::System { kind, data, .. }
                if kind == "session" && data["session_id"].as_str() == Some(saved.id.as_str()))))
            }),
    );
    runner.set_compactor(
        Arc::new(ModelCompactor {
            model: catalog_model.clone(),
            max_requests: app_config.compaction_max_requests,
        }),
        catalog_model.spec.auto_compact_at_tokens,
    );
    runner.set_observer(Arc::new(|event| {
        if let WorkflowEvent::Warning(message) = event {
            session_warning(&message);
        }
    }));

    Ok((
        Boot {
            app_config,
            catalog_model,
            preflight,
            session,
            _session_lock: session_lock,
            harness,
            runner,
        },
        sink,
    ))
}

fn build_model(
    catalog_model: &CatalogModel,
    harness: &Harness,
    debug_dump_api_requests: bool,
    effort: Effort,
) -> Result<
    (
        Arc<dyn generative_model::GenerativeModel>,
        Vec<myco::prelude::PreludeEntry>,
    ),
    String,
> {
    let mut backend_config = catalog_model.backend.clone();
    match &mut backend_config {
        BackendConfig::Anthropic(c) => {
            if debug_dump_api_requests {
                c.debug_dump_api_requests = true;
            }
            // Always enable thinking; effort controls how hard the model thinks.
            c.effort = Some(effort);
        }
        BackendConfig::OpenAIResponses(c) | BackendConfig::OpenAICompletions(c) => {
            if debug_dump_api_requests {
                c.debug_dump_api_requests = true;
            }
            c.effort = Some(effort);
        }
    }

    let (epilogue, prelude) = prompts::agent_prompt_epilogue();
    let model = generative_model::new(GenerativeModelConfig {
        model: catalog_model.spec.clone(),
        tools: harness.tool_specs(),
        system_prompt: [
            SYSTEM_PROMPT_PROLOGUE.to_string(),
            epilogue,
            prompts::model_stamp(&catalog_model.spec.key),
            prompts::auto_compact_notice(
                catalog_model.spec.auto_compact_at_tokens,
                catalog_model.spec.context_window_tokens,
            ),
        ]
        .join("\n"),
        backend_config,
    })
    .map_err(|error| format!("could not create model: {error}"))?;
    let model = myco::core::image_store::with_images(
        model,
        myco::core::image_store::ImageStore::for_profile()?,
    );
    Ok((model, prelude))
}

async fn select_runner_model(
    runner: &mut SessionRunner,
    harness: &Harness,
    catalog: &CatalogModel,
    effort: Effort,
    debug_dump_api_requests: bool,
    compaction_max_requests: usize,
) -> Result<(), String> {
    let (model, prelude) = build_model(catalog, harness, debug_dump_api_requests, effort)?;
    runner
        .set_model(
            model,
            myco::ModelInfo::from_spec(&catalog.spec, Some(effort)),
        )
        .await
        .map_err(|error| error.to_string())?;
    runner
        .agent_mut()
        .set_before_generation_notice(Some(myco::session_runtime::prelude_change_notices(prelude)));
    runner
        .agent_mut()
        .set_retry_policy(catalog.backend.retry_policy());
    runner
        .agent_mut()
        .set_context_window_tokens(catalog.spec.context_window_tokens);
    runner
        .agent_mut()
        .set_max_truncated_resumes(catalog.spec.max_truncated_resumes);
    runner.set_compactor(
        Arc::new(ModelCompactor {
            model: catalog.clone(),
            max_requests: compaction_max_requests,
        }),
        catalog.spec.auto_compact_at_tokens,
    );
    runner
        .runtime()
        .set_max_image_base64_bytes(catalog.spec.max_image_base64_bytes);
    Ok(())
}

fn print_launcher_help(topic: &str) {
    let topic = topic.trim();
    if topic.is_empty() {
        let mut cmd = Args::command();
        let _ = cmd.print_help();
        println!();
        println!();
        print!("{}", myco::manual::format_catalog());
        println!("Example: myco --help harness-ops");
        return;
    }
    match myco::manual::format_article(topic) {
        Ok(body) => {
            println!("{body}");
        }
        Err(e) => {
            eprintln!("myco: {e}");
            eprintln!("{}", myco::manual::format_catalog());
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_is_default_and_host_protocol_remains_available() {
        let args = Args::parse_from(["myco"]);
        assert_eq!(args.mode, Mode::Server);
        assert_eq!(args.port, 8765);
        assert!(args.bind.is_loopback());
        let host = Args::parse_from(["myco", "--mode", "host", "--name", "remote"]);
        assert_eq!(host.mode, Mode::Host);
        assert_eq!(host.name, "remote");
    }

    #[test]
    fn existing_web_launch_options_remain_aliases() {
        let args = Args::parse_from(["myco", "--web", "0", "--web-bind", "::1"]);
        assert_eq!(args.port, 0);
        assert!(args.bind.is_loopback());
    }

    #[test]
    fn server_listen_addresses_are_loopback_only() {
        for address in ["localhost", "127.0.0.1", "127.0.0.2", "::1"] {
            let args = Args::try_parse_from(["myco", "--bind", address]).unwrap();
            assert!(args.bind.is_loopback());
        }
        for flag in ["--bind", "--web-bind"] {
            for address in [
                "0.0.0.0",
                "::",
                "192.168.1.10",
                "2001:db8::1",
                "example.com",
                "::ffff:127.0.0.1",
            ] {
                let error = Args::try_parse_from(["myco", flag, address]).unwrap_err();
                assert!(error.to_string().contains("use an SSH tunnel"));
            }
        }
    }

    #[test]
    fn one_shot_and_terminal_modes_are_explicit() {
        let args = Args::try_parse_from(["myco", "-p", "task"]).unwrap();
        assert_eq!(args.print, Some(Some("task".into())));
        let args = Args::try_parse_from(["myco", "-p", "--resume", "id"]).unwrap();
        assert_eq!(args.print, Some(None));
        assert_eq!(args.resume.as_deref(), Some("id"));
        for mode in ["cli", "interactive"] {
            assert_eq!(Args::parse_from(["myco", "--mode", mode]).mode, Mode::Cli);
        }
        for flags in [
            vec!["-p", "task", "--mode", "host"],
            vec!["-p", "task", "--mode", "cli"],
            vec!["-p", "task", "--port", "0"],
            vec!["-p", "task", "--web-bind", "127.0.0.1"],
        ] {
            assert!(Args::try_parse_from(std::iter::once("myco").chain(flags)).is_err());
        }
    }

    #[test]
    fn legacy_terminal_flags_and_ambiguous_resume_are_rejected() {
        for flags in [
            vec!["--mode", "session-browser"],
            vec!["--resume"],
            vec!["--color", "always"],
            vec!["--wrap", "80"],
            vec!["--parent-session", "id"],
            vec!["--fork"],
        ] {
            assert!(Args::try_parse_from(std::iter::once("myco").chain(flags)).is_err());
        }
    }
}
