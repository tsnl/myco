use std::{
    fs,
    io::{IsTerminal, Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use clap::{CommandFactory, Parser, ValueEnum};
use myco::generative_model::{
    self, BackendConfig, CatalogModel, Content, Effort, GenerativeModelConfig,
};
use myco::host::HostWorker;
use myco::session::{
    ActiveSession, CompactWorkerError, ConsoleLog, Palette, RECENT_SESSION_LIMIT, Session,
    SessionListEntry, attachment_note, expand_image_attachments, format_session_detail,
    format_session_list_line, list_sessions, render_block, resolve_and_load_session,
    run_compact_worker,
};
use myco::tui::{ConsoleTuiSink, StdoutTuiSink, TuiProducer};
use myco::{
    Agent, AgentEvent, ColorMode, Config, ConfigUserSettings, EventSink, Harness,
    ListRecentService, SessionHistoryTool, SessionKind, SessionMetaTool, StartupPreflight,
    TraceContext, WrapMode, prompts,
};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::{DefaultHistory, History};
use rustyline::validate::Validator;
use rustyline::{
    Cmd, ConditionalEventHandler, Context, Editor, Event, EventContext, EventHandler, Helper,
    KeyCode, KeyEvent, Modifiers, RepeatCount,
};
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a helpful assistant running in an agentic harness with unfettered computer access.
"#;

const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/exit",
    "/quit",
    "/new",
    "/session",
    "/sessions",
    "/hosts",
    "/resume",
    "/effort",
    "/title",
    "/compact",
];

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Myco agent",
    long_about = None,
    // Custom `--help [ARTICLE]` (see `help_topic`); clap's built-in help flag is disabled.
    disable_help_flag = true,
)]
struct Args {
    /// Show CLI help, or print a manual article when ARTICLE is given
    /// (e.g. `myco --help overview`). Same articles as the `manual` host tool.
    #[arg(
        long = "help",
        short = 'h',
        value_name = "ARTICLE",
        num_args = 0..=1,
        default_missing_value = "",
        help = "Print help (or a manual article when ARTICLE is given)"
    )]
    help_topic: Option<String>,

    /// Run mode. `interactive` (default) starts the agent REPL; `host` runs the
    /// tool runtime, speaking NDJSON over stdin/stdout (spawned locally or via
    /// ssh); `session-browser` runs the standalone session picker.
    #[arg(long, value_enum, default_value_t = Mode::Interactive)]
    mode: Mode,

    /// Print mode (non-interactive): run one agent turn, stream the answer to
    /// stdout, and exit. Bare `-p` takes the prompt from piped stdin; with
    /// PROMPT given, piped stdin (if any) is prepended as context. Combines
    /// with `--resume` (continue a saved session) and `--parent-session` /
    /// `--fork` (one-shot nested agent).
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<Option<String>>,

    /// Write the picked session id to FILE instead of stdout. Only with
    /// `--mode session-browser`; used by the bare-/resume tmux popup handshake.
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// With `--mode session-browser`: rank sessions matching QUERY (keyword
    /// search over title/first message/scratchpad/console tail) instead of
    /// listing by recency.
    #[arg(long, value_name = "QUERY")]
    search: Option<String>,

    /// Host name advertised in hello_ok / logs. Only used with `--mode host`.
    #[arg(long, default_value = "local")]
    name: String,

    /// Model key from the config.toml [models] catalog.
    /// Default: `model` from config.toml, else the sole configured model.
    #[arg(long)]
    model: Option<String>,

    /// Dump provider request bodies to stderr
    #[arg(long)]
    debug_dump_api_requests: bool,

    /// Resume a saved session (id or unique prefix). Bare `--resume` reopens the most recent.
    #[arg(long)]
    resume: Option<Option<String>>,

    /// Run as a nested agent of the given supervisor session: the fresh session
    /// is created hidden (`kind: subagent`) with this parent recorded. Used when
    /// one myco drives another over a bash session (see `--help overview`).
    #[arg(long, value_name = "SESSION_ID")]
    parent_session: Option<String>,

    /// With --parent-session: context fork — seed the fresh child session with
    /// the parent's saved conversation instead of a blank context. Same
    /// `--model` as the parent keeps the fork on the parent's prompt cache.
    #[arg(long, requires = "parent_session")]
    fork: bool,

    /// Reasoning / extended-thinking effort (low|medium|high|max). Default: high.
    /// Change mid-session with `/effort`.
    #[arg(long, value_parser = parse_effort_arg, default_value = "high")]
    effort: Effort,

    /// Path to myco config (knobs; hosts come from ~/.ssh/config).
    /// Default: $MYCO_CONFIG or ~/.myco/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Color output (auto|always|never). Auto colors only when stdout is a TTY
    /// and respects NO_COLOR / CLICOLOR_FORCE.
    #[arg(long, value_name = "WHEN", value_parser = parse_color_arg, default_value_t = ColorMode::Auto)]
    color: ColorMode,

    /// Word-wrap prose (auto|off|COLS). The value caps the width: effective
    /// wrap is min(cap, terminal width), re-measured every prompt so resizes
    /// reflow (auto = 80). TTY only; code blocks and piped output never wrap.
    #[arg(long, value_name = "MODE", value_parser = parse_wrap_arg, default_value_t = WrapMode::Auto)]
    wrap: WrapMode,
}

fn parse_effort_arg(s: &str) -> Result<Effort, String> {
    s.parse()
}

fn parse_color_arg(s: &str) -> Result<ColorMode, String> {
    s.parse()
}

fn parse_wrap_arg(s: &str) -> Result<WrapMode, String> {
    s.parse()
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Agent REPL (default).
    Interactive,
    /// Tool runtime over stdin/stdout NDJSON.
    Host,
    /// Standalone fzf session picker; prints the chosen id, or writes it
    /// to `--out`.
    SessionBrowser,
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    if let Some(topic) = args.help_topic.as_deref() {
        print_cli_help(topic);
        return;
    }
    if args.print.is_some() && args.mode != Mode::Interactive {
        eprintln!("myco: -p/--print does not combine with --mode host/session-browser");
        std::process::exit(2);
    }
    match args.mode {
        Mode::Interactive if args.print.is_some() => run_print(args).await,
        Mode::Interactive => run_interactive(args).await,
        Mode::Host => run_host(args).await,
        Mode::SessionBrowser => run_session_browser(args),
    }
}

/// `--mode session-browser`: standalone picker, spawned by the bare-/resume
/// tmux popup or run directly.
fn run_session_browser(args: Args) {
    if let Err(e) = myco::session_browser::run(args.out.as_deref(), args.search.as_deref()) {
        eprintln!("session browser error: {e}");
        std::process::exit(1);
    }
}

/// `--mode host`: serve OS tools over stdin/stdout NDJSON. Used for **remote**
/// hosts (ssh … myco --mode host). The agent-side local host is in-process and
/// does not spawn this mode.
async fn run_host(args: Args) {
    if let Err(e) = HostWorker::standard(args.name).serve_stdio().await {
        eprintln!("myco host error: {e}");
        std::process::exit(1);
    }
}

/// `-p/--print`: one non-interactive agent turn. Answer text streams to stdout
/// raw (no sections, colors, or wrap); everything else — preflight WARNING,
/// `session=<id>`, errors — goes to stderr so stdout stays pipeable. The
/// session persists like an interactive run (`--resume` picks it up), and
/// `--parent-session` / `--fork` make it a one-shot nested agent.
async fn run_print(args: Args) {
    let arg = args.print.clone().flatten();
    let prompt = match assemble_print_prompt(arg.clone(), read_piped_stdin()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("myco: {e}");
            std::process::exit(2);
        }
    };
    // `@path.png` mentions attach images, same contract as the REPL; a bad
    // path is a usage error before any config/model work.
    let content = match print_turn_content(arg.as_deref(), prompt.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("myco: {e}");
            std::process::exit(2);
        }
    };
    if let Some(note) = attachment_note(&content) {
        eprintln!("{note}");
    }

    let (app_config, catalog_model) = resolve_app_config_or_exit(&args);
    let model_key = catalog_model.spec.key.clone();

    let preflight = StartupPreflight::run(&app_config.harness.remote_hosts);
    let mut warn = Vec::new();
    let _ = preflight.write_warning_section(&mut warn, Palette::plain());
    if !warn.is_empty() {
        let _ = std::io::stderr().write_all(&warn);
    }

    let active_session = ActiveSession::new(initial_session_or_exit(&args, &model_key));

    let session_tool =
        Arc::new(SessionMetaTool::new(active_session.clone())) as Arc<dyn myco::ToolService>;
    let history_tool = Arc::new(SessionHistoryTool::new()) as Arc<dyn myco::ToolService>;
    let list_recent_tool = Arc::new(ListRecentService::new()) as Arc<dyn myco::ToolService>;
    let harness = attach_harness_or_exit(
        &app_config,
        &preflight,
        vec![session_tool, history_tool, list_recent_tool],
    )
    .await;

    let model = build_model(
        &catalog_model,
        &harness,
        args.debug_dump_api_requests,
        args.effort,
    );
    let sink = Arc::new(PrintEventSink::default());
    let mut agent = Agent::new(model, harness, sink.clone());
    agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
    let restored = active_session.snapshot();
    agent.set_history(restored.messages.clone());
    agent.set_last_usage(restored.last_usage);
    // Mid-turn checkpoints, same as interactive: children forked off this run
    // see finished tool rounds, and a crash loses at most the in-flight round.
    wire_checkpoint(&mut agent, &active_session);

    if let Err(e) = active_session.maybe_auto_title_from_user_text(&prompt) {
        eprintln!("warning: could not auto-title session: {e}");
    }

    // Ctrl-C cancels the in-flight turn; history stays well-formed.
    let cancel = myco::CancelToken::new();
    let cancel_on_sigint = cancel.clone();
    let sigint_task = tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            cancel_on_sigint.cancel();
        }
    });

    let result = agent.interact(content, cancel).await;
    sigint_task.abort();
    sink.finish();

    // Persist whatever history the agent has, including failed/cancelled turns.
    if let Err(e) = persist_session(&agent, &active_session, /*force*/ true) {
        eprintln!("warning: could not save session: {e}");
    }
    if !agent.history().is_empty() || active_session.snapshot().json_path().exists() {
        eprintln!("session={}", active_session.id());
    }

    match result {
        Ok(_) => {}
        Err(myco::AgentInteractionError::Cancelled) => {
            eprintln!("(cancelled)");
            std::process::exit(130);
        }
        Err(e) => {
            eprintln!("myco: {e}");
            std::process::exit(1);
        }
    }
}

/// Combine the `-p` argument and piped stdin into the user prompt. With both,
/// stdin is context and the argument is the instruction, so
/// `git diff | myco -p "review this"` reads in that order.
fn assemble_print_prompt(arg: Option<String>, piped: Option<String>) -> Result<String, String> {
    let arg = arg.filter(|s| !s.trim().is_empty());
    let piped = piped.filter(|s| !s.trim().is_empty());
    match (piped, arg) {
        (Some(stdin), Some(arg)) => Ok(format!("{}\n\n{arg}", stdin.trim_end())),
        (None, Some(arg)) => Ok(arg),
        (Some(stdin), None) => Ok(stdin),
        (None, None) => Err("print mode needs a prompt: `myco -p \"…\"` or pipe stdin".into()),
    }
}

/// Content blocks for the print-mode turn: images from `@path` mentions in
/// the `-p` argument (matching the REPL's typed-input contract), then the
/// full prompt text. Piped stdin is data — it is never parsed for
/// attachments, so `git diff | myco -p "review"` cannot fail on a `@x.png`
/// that happens to appear in the diff.
fn print_turn_content(arg: Option<&str>, prompt: String) -> Result<Vec<Content>, String> {
    let mut content: Vec<Content> = expand_image_attachments(arg.unwrap_or(""))?
        .into_iter()
        .filter(|c| matches!(c, Content::Image { .. }))
        .collect();
    content.push(Content::Text { text: prompt });
    Ok(content)
}

/// Read piped stdin fully; `None` when stdin is a TTY or effectively empty.
fn read_piped_stdin() -> Option<String> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut buf = Vec::new();
    stdin.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    (!text.trim().is_empty()).then_some(text)
}

/// One explicit resolution step: model catalog (gateways/models + auth),
/// harness hosts and default model key (--config → $MYCO_CONFIG →
/// ~/.myco/config.toml), and the color decision. Everything downstream
/// reads this, not the env or config files.
fn resolve_app_config_or_exit(args: &Args) -> (Config, CatalogModel) {
    let app_config = Config::resolve(ConfigUserSettings {
        config_path: args.config.clone(),
        model: args.model.clone(),
        color: args.color,
        wrap: args.wrap,
        ..Default::default()
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

/// Starting session for an agent run (interactive or print): `--resume` loads
/// a saved session; otherwise a fresh one, hidden and parented under
/// `--parent-session`, context-forked with `--fork`.
fn initial_session_or_exit(args: &Args, model_key: &str) -> Session {
    if args.resume.is_some() && args.parent_session.is_some() {
        // A resumed session already carries its kind/parent; rewriting them
        // here would silently change a stored session's identity.
        eprintln!("--parent-session applies only to a fresh session; drop it when resuming");
        std::process::exit(2);
    }
    match &args.resume {
        Some(id_opt) => load_resume_session_or_exit(id_opt.as_deref()),
        None => {
            let mut fresh = Session::new(model_key.to_string());
            if let Some(parent) = args.parent_session.as_deref() {
                let parent = parent.trim();
                if parent.is_empty() {
                    eprintln!("--parent-session needs a non-empty session id");
                    std::process::exit(2);
                }
                if args.fork {
                    // Context fork: seed the child with the parent's saved
                    // conversation. Loading validates the parent (unlike the
                    // record-only path below) and normalizes a prefix to the
                    // full id.
                    match Session::load_by_id_or_prefix(parent) {
                        Ok(parent_session) => {
                            fresh = parent_session.fork_child(model_key.to_string());
                        }
                        Err(e) => {
                            eprintln!("--fork: cannot load parent session {parent:?}: {e}");
                            std::process::exit(1);
                        }
                    }
                } else {
                    fresh.kind = SessionKind::Subagent;
                    fresh.parent_session_id = Some(parent.to_string());
                }
            }
            fresh
        }
    }
}

async fn attach_harness_or_exit(
    app_config: &Config,
    preflight: &StartupPreflight,
    root_services: Vec<Arc<dyn myco::ToolService>>,
) -> Arc<Harness> {
    Harness::attach_with_root_services(app_config.harness.clone(), root_services)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to attach harness: {e}");
            eprintln!(
                "hint: remote hosts come from ~/.ssh/config Host aliases; local needs no binary spawn"
            );
            if !preflight.executables.is_clean() {
                let names: Vec<&str> = preflight
                    .executables
                    .missing
                    .iter()
                    .map(|m| m.name)
                    .collect();
                eprintln!("hint: missing executables: {}", names.join(", "));
            }
            if preflight.ssh.has_problems() {
                eprintln!(
                    "hint: ssh-agent preflight reported missing keys or an unreachable agent; \
                     try `ssh-add -l` and `ssh-add --apple-use-keychain <key>`"
                );
            }
            eprintln!("config: {}", app_config.config_path.display());
            std::process::exit(1);
        })
}

/// Persist agent history at replayable mid-turn boundaries (after the user
/// message, after each completed tool round) so context forks and crash
/// recovery see the freshest well-formed snapshot.
fn wire_checkpoint(agent: &mut Agent, active_session: &ActiveSession) {
    let checkpoint_session = active_session.clone();
    agent.set_checkpoint(Box::new(move |messages, last_usage| {
        if let Err(e) = checkpoint_session.persist_messages(messages, last_usage, false) {
            eprintln!("warning: mid-turn session save failed: {e}");
        }
    }));
}

async fn run_interactive(args: Args) {
    let (app_config, catalog_model) = resolve_app_config_or_exit(&args);
    let model_key = catalog_model.spec.key.clone();
    let colors = app_config.colors_enabled;
    let wrap = effective_wrap_width(app_config.wrap_max);

    // Startup preflight: verify expected executables resolve (bash, tmux, fzf;
    // OpenSSH tools when remotes are configured), then unlock SSH identities
    // via the existing ssh-agent before attach — remote hosts use
    // `ssh -o BatchMode=yes` (NDJSON pipe is not a TTY), so OpenSSH must never
    // need to prompt on the host pipe.
    // Problems are printed after the banner (WARNING block), not here.
    let preflight = StartupPreflight::run(&app_config.harness.remote_hosts);

    // Session handle first so `session_meta` can share it with the agent harness.
    let resuming = args.resume.is_some();
    let initial_session = initial_session_or_exit(&args, &model_key);
    let active_session = ActiveSession::new(initial_session);

    // The Ui: one producer owning stdout and the plain-text console mirror
    // ({id}.console, TTY-gated like colors/wrap). The mirror resolves the
    // current session id per append, so /new, /compact, /resume redirect it
    // automatically. The agent can read it to see exactly what the user saw,
    // including live-only WARNING/ERROR sections and meta-command output.
    let ui = Arc::new(TuiProducer::new(
        Arc::new(StdoutTuiSink { colors }),
        Arc::new(ConsoleTuiSink::new(ConsoleLog::new(
            active_session.clone(),
            app_config.stdout_is_tty,
        ))),
        colors,
        wrap,
    ));

    let session_tool =
        Arc::new(SessionMetaTool::new(active_session.clone())) as Arc<dyn myco::ToolService>;
    let history_tool = Arc::new(SessionHistoryTool::new()) as Arc<dyn myco::ToolService>;
    let list_recent_tool = Arc::new(ListRecentService::new()) as Arc<dyn myco::ToolService>;
    let harness = attach_harness_or_exit(
        &app_config,
        &preflight,
        vec![session_tool, history_tool, list_recent_tool],
    )
    .await;
    // Thinking/reasoning is always requested; UI shows summary lines only (not stored).
    let effort = args.effort;
    let debug_dump_api_requests = args.debug_dump_api_requests;
    let model = build_model(&catalog_model, &harness, debug_dump_api_requests, effort);
    let mut agent = Agent::new(model, harness.clone(), ui.clone());
    agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
    let restored = active_session.snapshot();
    agent.set_history(restored.messages.clone());
    agent.set_last_usage(restored.last_usage);
    // Mid-turn checkpoints; the end-of-turn force-save in run_user_turn stays
    // the backstop.
    wire_checkpoint(&mut agent, &active_session);
    let ctrl_l = Arc::new(AtomicBool::new(false));
    let mut editor = build_editor(ctrl_l.clone());

    load_readline_history(&mut editor, &active_session);

    let session_label = active_session.with(|s| match &s.title {
        Some(t) if !t.is_empty() => format!("{} \"{t}\"", s.id),
        _ => s.id.clone(),
    });
    // Startup chrome: headed banner block (rule, MYCO, model/session, key
    // hints). Hosts via /hosts, effort via /effort, config path via
    // attach-failure hints.
    ui.startup_banner(&model_key, &session_label);
    // Preflight problems (missing executables, ssh-agent) open one WARNING
    // block after the banner, before the first USER block; happy path silent.
    if preflight.has_problems() {
        ui.warning_section(&preflight.warning_body());
    }
    // Blank line closes the startup chrome before the first USER rule
    // (or the resumed-history replay).
    ui.blank_line();
    if resuming {
        ui.replay_history(agent.history());
    }

    let mut repl = ReplSession {
        agent,
        session: active_session,
        editor,
        harness,
        catalog_model,
        effort,
        debug_dump_api_requests,
        ctrl_l,
        wrap,
        wrap_max: app_config.wrap_max,
        repaint: app_config.repaint_enabled,
        ui: ui.clone(),
        turn_cancel: TurnCancel::default(),
    };
    repl.run_repl().await;

    if let Err(e) = persist_session(&repl.agent, &repl.session, /*force*/ true) {
        eprintln!("warning: could not save session on exit: {e}");
    }
    if let Err(e) = save_readline_history(&mut repl.editor, &repl.session) {
        eprintln!("warning: could not save history on exit: {e}");
    }
    // Only announce a session id if we actually wrote one (non-empty history).
    if !repl.agent.history().is_empty() || repl.session.snapshot().json_path().exists() {
        ui.line(&format!("session={}", repl.session.id()));
    }
}

fn build_model(
    catalog_model: &CatalogModel,
    harness: &Harness,
    debug_dump_api_requests: bool,
    effort: Effort,
) -> Arc<dyn generative_model::GenerativeModel> {
    let mut backend_config = catalog_model.backend.clone();
    match &mut backend_config {
        BackendConfig::Anthropic(c) => {
            if debug_dump_api_requests {
                c.debug_dump_api_requests = true;
            }
            // Always enable thinking; effort controls how hard the model thinks.
            c.effort = Some(effort);
        }
        BackendConfig::OpenAIResponses(c) => {
            if debug_dump_api_requests {
                c.debug_dump_api_requests = true;
            }
            c.effort = Some(effort);
        }
    }

    generative_model::new(GenerativeModelConfig {
        model: catalog_model.spec.clone(),
        tools: harness.tool_specs(),
        system_prompt: [
            SYSTEM_PROMPT_PROLOGUE.to_string(),
            prompts::agent_prompt_epilogue(),
            prompts::model_stamp(&catalog_model.spec.key),
        ]
        .join("\n"),
        backend_config,
    })
    .unwrap_or_else(|e| {
        eprintln!("Failed to create model: {e}");
        std::process::exit(1);
    })
}

/// Ctrl-L handler: when the input buffer is empty, submit an empty line and
/// signal the REPL loop (via the shared flag) to clear scrollback and reprint
/// the conversation. When the buffer has text, fall back to rustyline's default
/// (clear visible screen, keep the typed line) so we never discard input.
struct CtrlLHandler {
    flag: Arc<AtomicBool>,
}

impl ConditionalEventHandler for CtrlLHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        if ctx.line().is_empty() {
            self.flag.store(true, Ordering::SeqCst);
            Some(Cmd::AcceptLine)
        } else {
            None
        }
    }
}

fn build_editor(ctrl_l: Arc<AtomicBool>) -> Editor<ReplHelper, DefaultHistory> {
    let mut editor = Editor::<ReplHelper, DefaultHistory>::new().unwrap_or_else(|e| {
        eprintln!("Failed to init readline: {e}");
        std::process::exit(1);
    });
    editor.set_helper(Some(ReplHelper));
    // Multiline: insert a newline without submitting. Enter still accepts the buffer.
    // Alt-Enter arrives as ESC+CR and Ctrl-J as 0x0A, so both are distinguishable
    // in any terminal. Shift-Enter is bound too, but most terminals transmit it as
    // plain CR -- identical to Enter, so it submits instead; the binding only fires
    // on the Windows console, whose API reports modifiers. Advertise Alt-Enter.
    // (EventHandler is not Clone, so each bind gets its own Simple(Cmd::Newline).)
    editor.bind_sequence(
        KeyEvent(KeyCode::Enter, Modifiers::ALT),
        EventHandler::Simple(Cmd::Newline),
    );
    editor.bind_sequence(
        KeyEvent(KeyCode::Enter, Modifiers::SHIFT),
        EventHandler::Simple(Cmd::Newline),
    );
    // Override the default AcceptOrInsertLine mapping so Ctrl-J inserts a newline.
    editor.bind_sequence(KeyEvent::ctrl('J'), EventHandler::Simple(Cmd::Newline));
    // Ctrl-L clears scrollback + reprints the transcript when the buffer is
    // empty (see CtrlLHandler).
    editor.bind_sequence(
        KeyEvent::ctrl('L'),
        EventHandler::Conditional(Box::new(CtrlLHandler { flag: ctrl_l })),
    );
    editor
}

fn load_resume_session_or_exit(id_or_prefix: Option<&str>) -> Session {
    match resolve_and_load_session(id_or_prefix) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("Failed to resume session: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// REPL
// ---------------------------------------------------------------------------

/// Everything the interactive REPL threads through its turn / meta / compact
/// helpers: the agent with its live session and line editor, the resolved
/// model and UI handles, and the REPL-scoped knobs.
struct ReplSession {
    agent: Agent,
    session: ActiveSession,
    editor: Editor<ReplHelper, DefaultHistory>,
    harness: Arc<Harness>,
    catalog_model: CatalogModel,
    effort: Effort,
    debug_dump_api_requests: bool,
    ctrl_l: Arc<AtomicBool>,
    /// Wrap width resolved at startup; re-measured every prompt.
    wrap: Option<usize>,
    wrap_max: Option<usize>,
    repaint: bool,
    ui: Arc<TuiProducer>,
    turn_cancel: TurnCancel,
}

impl ReplSession {
    async fn run_repl(&mut self) {
        let sigint_listener = self.turn_cancel.install();
        let mut last_wrap = self.wrap;
        loop {
            // Re-measure the terminal each prompt: after a resize, reflow the
            // whole dialog at the new width (same clear+reprint as Ctrl-L). This
            // is the safe point — never mid-stream, never while rustyline owns
            // the terminal. Dumb terminals skip the reprint (no cursor codes)
            // but still pick up the new width for subsequent turns. The reflow is
            // a redraw, not new content, so it is not mirrored to the console.
            let wrap = effective_wrap_width(self.wrap_max);
            if wrap != last_wrap {
                last_wrap = wrap;
                self.ui.set_wrap(wrap);
                if self.repaint && !self.agent.history().is_empty() {
                    clear_and_reprint(&self.agent, &self.ui);
                }
            }
            let max = self.agent.context_window_tokens();
            let usage = self.agent.last_usage();
            // `None` (→ `?`) = resumed before usage was tracked; `0` = genuinely empty session.
            let used = match usage {
                Some(u) => Some(u.context_tokens()),
                None if self.agent.history().is_empty() => Some(0),
                None => None,
            };
            let running = self
                .harness
                .running_tool_summaries(self.agent.context().agent_id);
            self.ui.user_header(used, max, usage, &running);
            // No "> " prefix; body is typed on the line after the USER header.
            // Multiline: Alt-Enter / Ctrl-J inserts a newline in-buffer; plain Enter
            // submits the whole buffer to the agent.
            let line = match self.editor.readline("") {
                Ok(l) => l,
                Err(ReadlineError::Interrupted) => {
                    // Ctrl-C cancels the current (multi-)line, keeps the session open.
                    continue;
                }
                Err(ReadlineError::Eof) => break, // Ctrl-D
                Err(e) => {
                    eprintln!("Readline error: {e}");
                    break;
                }
            };

            // Ctrl-L on an empty buffer submits an empty line + sets this flag:
            // clear scrollback and reprint the conversation.
            if self.ctrl_l.swap(false, Ordering::SeqCst) {
                clear_and_reprint(&self.agent, &self.ui);
                continue;
            }

            let input = line.trim().to_string();
            if input.is_empty() {
                continue;
            }
            reprint_input_wrapped(&line, last_wrap, self.repaint);
            // Mirror the submitted line once (wrap-only, as shown). rustyline echoed
            // it to the terminal but not to us; the re-echo above only moves the
            // cursor, so the console needs the logical text here.
            self.ui.submitted_input(&line);
            if is_exit_command(&input) {
                break;
            }
            if let Some(cmd) = parse_meta(&input) {
                if matches!(cmd, MetaCommand::Compact) {
                    self.run_compact().await;
                    continue;
                }
                self.handle_meta(cmd);
                continue;
            }

            self.run_user_turn(input).await;
        }
        sigint_listener.abort();
    }
}

/// Effective wrap width right now: the configured cap bounded by the measured
/// terminal width. `None` = wrap off (includes non-TTY stdout, resolved at
/// startup). Cheap (one ioctl) — called once per prompt.
fn effective_wrap_width(wrap_max: Option<usize>) -> Option<usize> {
    let cap = wrap_max?;
    match myco::config::detect_terminal_size() {
        Some((cols, _)) => Some(cap.min(cols)),
        None => Some(cap),
    }
}

/// Visual rows the just-submitted input echo occupies: terminal character
/// wrap at `cols`, one row minimum per logical line.
fn input_echo_rows(line: &str, cols: usize) -> usize {
    line.split('\n')
        .map(|l| l.width().div_ceil(cols).max(1))
        .sum()
}

/// Replace the just-submitted input echo with a word-wrapped copy.
///
/// The rustyline edit buffer is the one region the CLI repaints (the user can
/// backspace while editing); this closes that exception at submit time —
/// after this, output is append-only again. Wrap-only, no markdown styling:
/// the user's words stay exactly as typed. Skipped when wrap is off, repaint
/// is unavailable (non-TTY stdout or `TERM=dumb`), the echo may have
/// scrolled off-screen, or wrapping would change nothing.
fn reprint_input_wrapped(line: &str, wrap: Option<usize>, repaint: bool) {
    if wrap.is_none() || !repaint {
        return;
    }
    let Some((cols, screen_rows)) = myco::config::detect_terminal_size() else {
        return;
    };
    let wrapped = render_block(line, Palette::plain().with_wrap(wrap));
    if wrapped == line {
        return;
    }
    let rows = input_echo_rows(line, cols);
    if rows >= screen_rows {
        return;
    }
    print!("\x1b[{rows}A\x1b[J");
    println!("{wrapped}");
    let _ = std::io::stdout().flush();
}

/// SIGINT → in-flight turn cancellation, REPL-scoped.
///
/// One persistent `ctrl_c()` listener cancels whichever token is currently
/// armed. Any code that awaits a model turn (user turns, the compact worker)
/// arms its token first and disarms after, so Ctrl-C always reaches the work
/// in flight. A per-turn spawned/aborted listener is not enough: tokio's
/// SIGINT handler stays installed process-wide after the first install, so a
/// later await with no live listener (the old `/compact` path) swallowed
/// Ctrl-C entirely. At the prompt rustyline owns the terminal in raw mode
/// (^C arrives as a key, not a signal) and no token is armed.
#[derive(Clone, Default)]
struct TurnCancel {
    slot: Arc<std::sync::Mutex<Option<myco::CancelToken>>>,
}

impl TurnCancel {
    /// Spawn the process-wide SIGINT listener feeding this slot.
    fn install(&self) -> tokio::task::JoinHandle<()> {
        let slot = self.clone();
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break;
                }
                if let Some(token) = slot.slot.lock().unwrap().as_ref() {
                    token.cancel();
                }
            }
        })
    }

    fn arm(&self) -> myco::CancelToken {
        let token = myco::CancelToken::new();
        *self.slot.lock().unwrap() = Some(token.clone());
        token
    }

    fn disarm(&self) {
        *self.slot.lock().unwrap() = None;
    }
}

impl ReplSession {
    async fn run_user_turn(&mut self, input: String) {
        let _ = self.editor.add_history_entry(&input);
        if let Err(e) = save_readline_history(&mut self.editor, &self.session) {
            eprintln!("warning: could not save history: {e}");
        }
        // `@path.png` mentions attach images. A bad path aborts the turn before
        // the model is called (headed ERROR section, like generate failures) so
        // the user can fix the path and resubmit — nothing is silently dropped.
        let content = match expand_image_attachments(&input) {
            Ok(c) => c,
            Err(e) => {
                self.ui.error_section(&e);
                self.ui.blank_line();
                return;
            }
        };
        // Same note, same position as replay: directly under the wrapped input.
        if let Some(note) = attachment_note(&content) {
            self.ui.line(&note);
        }
        if let Err(e) = self.session.maybe_auto_title_from_user_text(&input) {
            eprintln!("warning: could not auto-title session: {e}");
        }

        let cancel = self.turn_cancel.arm();

        // First assistant section opens with its own blank line + thin rule + header.
        match self.agent.interact(content, cancel).await {
            Ok(_) => self.ui.blank_line(),
            Err(myco::AgentInteractionError::Cancelled) => self.ui.cancelled(),
            Err(e) => {
                // Headed ERROR section (the agent already closed the ASSISTANT
                // stream via TurnFinished). Generate failures (context overflow,
                // provider errors) are live-only — not stored in session history —
                // so resume/Ctrl-L will not replay them.
                self.ui.error_section(&e.to_string());
                self.ui.blank_line();
            }
        }

        self.turn_cancel.disarm();

        // Persist whatever history the agent has, including failed/cancelled turns.
        if let Err(e) = persist_session(&self.agent, &self.session, /*force*/ true) {
            eprintln!("warning: could not save session: {e}");
        }
        if let Err(e) = save_readline_history(&mut self.editor, &self.session) {
            eprintln!("warning: could not save history: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

impl ReplSession {
    /// `/compact`: run the worker lifecycle (see
    /// [`myco::session::run_compact_worker`]) and switch the live REPL to the
    /// successor it built.
    async fn run_compact(&mut self) {
        if let Err(e) =
            self.session
                .persist_messages(self.agent.history(), self.agent.last_usage(), true)
        {
            eprintln!("compact: failed to persist current session: {e}");
            return;
        }
        let predecessor = self.session.snapshot();
        if predecessor.messages.is_empty() {
            eprintln!("compact: session is empty");
            return;
        }

        self.ui
            .line(&format!("compacting session={} …", predecessor.id));

        // Ctrl-C during compaction cancels the worker turn like any user turn.
        let cancel = self.turn_cancel.arm();
        let result = run_compact_worker(
            &predecessor,
            &self.catalog_model,
            self.harness.clone(),
            cancel,
        )
        .await;
        self.turn_cancel.disarm();

        let (successor, outcome) = match result {
            Ok(v) => v,
            Err(CompactWorkerError::Cancelled) => {
                self.ui.line("compact: cancelled (session unchanged)");
                return;
            }
            Err(CompactWorkerError::Failed(reason)) => {
                eprintln!("compact: {reason}");
                return;
            }
        };

        // Switch live REPL to successor.
        if let Err(e) = save_readline_history(&mut self.editor, &self.session) {
            eprintln!("warning: could not save history: {e}");
        }
        self.session.replace(successor.clone());
        self.agent.set_history(successor.messages.clone());
        self.agent.set_last_usage(successor.last_usage);
        reload_readline_history(&mut self.editor, &self.session);
        clear_and_reprint(&self.agent, &self.ui);
        self.ui.line(&format!(
            "compacted → new session={}  from={}  kept_tail={} messages  summary={}",
            outcome.successor_id,
            outcome.predecessor_id,
            outcome.tail_messages,
            outcome.summary_path.display()
        ));
    }
}

// ---------------------------------------------------------------------------
// Meta-commands
// ---------------------------------------------------------------------------

enum MetaCommand<'a> {
    Help,
    New,
    Session,
    Sessions,
    Hosts,
    Resume(Option<&'a str>),
    /// `None` → print current effort; `Some` → set effort.
    Effort(Option<&'a str>),
    Title(Option<&'a str>),
    Compact,
}

fn is_exit_command(input: &str) -> bool {
    matches!(
        input,
        "exit" | "quit" | ":q" | "/exit" | "/quit" | ":exit" | ":quit"
    )
}

fn parse_meta(input: &str) -> Option<MetaCommand<'_>> {
    let input = input.trim();
    let (head, rest) = match input.split_once(char::is_whitespace) {
        Some((h, r)) => (h, Some(r.trim())),
        None => (input, None),
    };

    // Accept `/cmd`, `:cmd`, and bare `help`.
    let cmd = head.strip_prefix('/').or_else(|| head.strip_prefix(':'));
    match (cmd, rest) {
        (Some("help"), _) => Some(MetaCommand::Help),
        (None, _) if head == "help" => Some(MetaCommand::Help),
        (Some("new"), _) => Some(MetaCommand::New),
        (Some("session"), _) => Some(MetaCommand::Session),
        (Some("sessions"), _) => Some(MetaCommand::Sessions),
        (Some("hosts"), _) => Some(MetaCommand::Hosts),
        (Some("resume"), arg) => Some(MetaCommand::Resume(arg.filter(|s| !s.is_empty()))),
        (Some("effort"), arg) => Some(MetaCommand::Effort(arg.filter(|s| !s.is_empty()))),
        (Some("title"), arg) => Some(MetaCommand::Title(arg)),
        (Some("compact"), _) => Some(MetaCommand::Compact),
        _ if head.starts_with('/') || head.starts_with(':') => {
            eprintln!("Unknown command: {head}  (try /help)");
            Some(MetaCommand::Help)
        }
        _ => None,
    }
}

impl ReplSession {
    fn handle_meta(&mut self, cmd: MetaCommand<'_>) {
        match cmd {
            MetaCommand::Help => print_help(&self.ui),
            MetaCommand::Session => {
                let _ = self.session.persist_messages(
                    self.agent.history(),
                    self.agent.last_usage(),
                    false,
                );
                self.ui
                    .text(&format_session_detail(&self.session.snapshot()));
            }
            MetaCommand::Sessions => match list_sessions(0) {
                Ok(list) => {
                    let shown = RECENT_SESSION_LIMIT.min(list.len());
                    print_session_list(&list[..shown], &self.ui);
                    if list.len() > shown {
                        self.ui.line(&format!(
                            "  … {} more — bare /resume opens the session browser",
                            list.len() - shown
                        ));
                    }
                }
                Err(e) => eprintln!("Failed to list sessions: {e}"),
            },
            MetaCommand::Hosts => print_host_status(&self.harness, &self.ui),
            MetaCommand::New => {
                self.save_before_switch();
                // A nested run stays nested across /new: carry kind + parent lineage.
                let snapshot = self.session.snapshot();
                let mut fresh = Session::new(self.catalog_model.spec.key.clone());
                fresh.kind = snapshot.kind;
                fresh.parent_session_id = snapshot.parent_session_id.clone();
                self.session.replace(fresh);
                self.agent.set_history(Vec::new());
                self.agent.set_last_usage(None);
                reload_readline_history(&mut self.editor, &self.session);
                // Fresh canvas for a fresh session (same clear as Ctrl-L, empty history).
                clear_and_reprint(&self.agent, &self.ui);
                self.ui.line(&format!("new session={}", self.session.id()));
            }
            MetaCommand::Resume(arg) => {
                self.save_before_switch();
                match resolve_resume_session(arg) {
                    Ok(loaded) => {
                        self.install_session(&loaded);
                        self.ui.line(&format!(
                            "resumed session={}  messages={}",
                            self.session.id(),
                            self.agent.history().len()
                        ));
                        self.ui.replay_history(self.agent.history());
                    }
                    Err(e) if e == RESUME_CANCELLED => self.ui.line("resume cancelled"),
                    Err(e) => eprintln!("resume failed: {e}"),
                }
            }
            MetaCommand::Effort(arg) => match arg {
                None => self
                    .ui
                    .line(&format!("effort={}  (low|medium|high|max)", self.effort)),
                Some(s) => match s.parse::<Effort>() {
                    Ok(next) if next == self.effort => self
                        .ui
                        .line(&format!("effort={}  (unchanged)", self.effort)),
                    Ok(next) => {
                        self.effort = next;
                        let model = build_model(
                            &self.catalog_model,
                            &self.harness,
                            self.debug_dump_api_requests,
                            self.effort,
                        );
                        self.agent.set_model(model);
                        self.agent.set_context_window_tokens(
                            self.catalog_model.spec.context_window_tokens,
                        );
                        self.ui.line(&format!("effort={}", self.effort));
                    }
                    Err(e) => eprintln!("{e}"),
                },
            },
            MetaCommand::Title(arg) => match arg {
                None => {
                    let snap = self.session.snapshot();
                    match snap.title.as_deref() {
                        Some(t) if !t.is_empty() => self.ui.line(&format!("title={t:?}")),
                        _ => self.ui.line("title=(none)"),
                    }
                }
                Some(t) if t.trim().is_empty() => {
                    if let Err(e) = self.session.with_mut(|s| {
                        s.set_title(None)?;
                        s.touch();
                        s.save()
                    }) {
                        eprintln!("failed to clear title: {e}");
                    } else {
                        self.ui.line("title=(none)");
                    }
                }
                Some(t) => {
                    if let Err(e) = self.session.with_mut(|s| {
                        s.set_title(Some(t.to_string()))?;
                        s.touch();
                        s.save()
                    }) {
                        eprintln!("failed to set title: {e}");
                    } else if let Some(title) = self.session.snapshot().title {
                        self.ui.line(&format!("title={title:?}"));
                    }
                }
            },
            MetaCommand::Compact => {
                // Handled asynchronously in run_repl.
            }
        }
    }

    fn save_before_switch(&mut self) {
        if let Err(e) = persist_session(&self.agent, &self.session, /*force*/ false) {
            eprintln!("warning: could not save current session: {e}");
        }
        if let Err(e) = save_readline_history(&mut self.editor, &self.session) {
            eprintln!("warning: could not save history: {e}");
        }
    }

    /// Make `loaded` the live session: swap it into the shared handle, reset
    /// agent history/usage, and reload readline history.
    fn install_session(&mut self, loaded: &Session) {
        self.session.replace(loaded.clone());
        self.agent.set_history(loaded.messages.clone());
        self.agent.set_last_usage(loaded.last_usage);
        reload_readline_history(&mut self.editor, &self.session);
    }
}

fn print_cli_help(topic: &str) {
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

fn print_help(ui: &TuiProducer) {
    ui.line(
        "\
Commands:
  /help                 Show this help
  /session              Show session metadata (title, links, scratchpad, path)
  /sessions             List recent sessions (title + link counts)
  /hosts                List configured hosts and attach status
  /resume [id|prefix]   Resume a session (no arg: fzf browser, as a tmux
                        popup when inside tmux)
  /new                  Start a new session (saves current; clears display)
  /effort [level]       Show or set reasoning effort (low|medium|high|max)
  /title [text]         Show or set session title (empty text clears)
  /compact              Compact context into a successor session (summary + tail)
  /exit, /quit          Save and quit  (also: exit, quit, :q, Ctrl-D)

Shortcuts:
  Enter                 Submit the current buffer
  Alt-Enter / Ctrl-J    Insert a newline (multiline input)
                        Note: most terminals send Shift-Enter as plain Enter,
                        which submits -- use Alt-Enter or Ctrl-J instead.
                        (Shift-Enter does insert a newline on the Windows
                        console, which reports modifiers.)
  Ctrl-C                Cancel current line at prompt; cancel in-flight turn while running
  Ctrl-L                Clear scrollback and reprint the conversation (empty prompt only)
  Ctrl-D                Save and quit

Images:
  Mention @path in a message to attach that image file as model input, e.g.
  `what is wrong here? @ui/shot.png`. Extensions png/jpg/jpeg/gif/webp, up to
  5 MiB each, `~/` expands, paths with spaces unsupported. The text is sent as
  typed; a bad path errors before the model is called.

Thinking/reasoning is always requested (default effort=high). The UI shows a
`Thinking: …` summary inside ASSISTANT; it is stored in session history for
resume but stripped from provider requests. Change effort with `/effort`.
Generate failures open a headed ERROR section (live only; not in history).

Each USER header shows `USER <used>/<max> (<pct>%)` context tokens, compact
(`63.8k/200k`; 0/max until the provider reports usage). A `⚙`-prefixed line
carries the finished turn's token counts (input = final request's prompt,
output summed across the turn); below it, one `●`-prefixed line per
still-running tool (live bash session on the in-process local host) shows
its command, uptime, and idle time.

Hosts:
  Local is always enabled in-process (no subprocess). Remotes come from
  ~/.ssh/config (Includes followed): every concrete Host alias is a lazy
  `ssh <alias> myco --mode host` remote. ~/.myco/config.toml (or --config /
  $MYCO_CONFIG) holds the model catalog ([gateways]/[models], default `model`)
  and knobs (attach_timeout_secs). Auth per entry: a literal
  token string or a source table (env var / file / none); see --help overview.
  Host tools accept optional input field `host` (default: local).
  Sessions (bash) are per-host. Use /hosts to list hosts and attach status
  (startup no longer prints them).
  Startup runs an ssh-agent preflight for remotes (BatchMode cannot prompt for
  passphrases on the NDJSON pipe). It is silent when clean; problems open a
  WARNING block. Missing keys: ssh-add, then restart.

Sessions are conversation memory only; shell/file state is not restored.
Empty sessions (no messages) are not written to disk.
On generate error after tools, history keeps user + assistant(tool_use) +
tool_results (well-formed for resume). Cancel mid-tools records synthetic
cancelled results for every tool_use.",
    );
}

fn print_host_status(harness: &Harness, ui: &TuiProducer) {
    let statuses = harness.host_status();
    if statuses.is_empty() {
        ui.line("hosts: (none)");
        return;
    }
    ui.line(&format!(
        "hosts: default=local  ({} total; local always in-process)",
        statuses.len()
    ));
    for s in statuses {
        // Local: always ok/in-process. Remotes: idle until first tool use; ok while
        // connected; DOWN after connect error.
        let state = if s.in_process || s.connected {
            "ok"
        } else if s.error.is_some() {
            "DOWN"
        } else {
            "idle"
        };
        let tools = if s.tools.is_empty() {
            "-".into()
        } else {
            s.tools.join(",")
        };
        let cmd = if s.command.is_empty() {
            String::new()
        } else {
            format!("  cmd={}", s.command.join(" "))
        };
        match &s.error {
            Some(err) => ui.line(&format!(
                "  [{state}] {}  tools={tools}{cmd}  err={err}",
                s.name
            )),
            None => ui.line(&format!("  [{state}] {}  tools={tools}{cmd}", s.name)),
        }
    }
}

// ---------------------------------------------------------------------------
// Session persistence (agent history ↔ Session file / readline history)
// ---------------------------------------------------------------------------

/// Copy agent history into `session` and write it when needed.
///
/// Empty sessions (no messages) are never written — this avoids littering
/// `~/.myco/session` with stubs from `/new` or quit-without-chat.
fn persist_session(agent: &Agent, session: &ActiveSession, force: bool) -> Result<(), String> {
    let history = agent.history();
    // Never create a file for a session that has never had a turn.
    if history.is_empty() && !session.snapshot().json_path().exists() {
        return Ok(());
    }
    session.persist_messages(history, agent.last_usage(), force)
}

fn save_readline_history(
    editor: &mut Editor<ReplHelper, DefaultHistory>,
    session: &ActiveSession,
) -> Result<(), String> {
    let history_path = session.with(|s| s.history_path());
    // Skip creating empty ~/.myco/session trees for sessions that never accepted input.
    if editor.history().is_empty() && !history_path.exists() {
        return Ok(());
    }
    if let Some(parent) = history_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    editor
        .save_history(&history_path)
        .map_err(|e| e.to_string())
}

fn load_readline_history(editor: &mut Editor<ReplHelper, DefaultHistory>, session: &ActiveSession) {
    let history_path = session.with(|s| s.history_path());
    if let Err(e) = editor.load_history(&history_path)
        && history_path.exists()
    {
        eprintln!("warning: could not load readline history: {e}");
    }
}

fn reload_readline_history(
    editor: &mut Editor<ReplHelper, DefaultHistory>,
    session: &ActiveSession,
) {
    editor.clear_history().ok();
    load_readline_history(editor, session);
}

// ---------------------------------------------------------------------------
// Resume: resolve id → load Session → install into agent/editor
// ---------------------------------------------------------------------------

/// Sentinel from the pickers: the user backed out, not a failure.
const RESUME_CANCELLED: &str = "cancelled";

fn resolve_resume_session(id_or_prefix: Option<&str>) -> Result<Session, String> {
    match id_or_prefix {
        Some(id) => Session::load_by_id_or_prefix(id),
        None => {
            let choice = if myco::session_browser::inside_tmux() {
                match myco::session_browser::pick_via_tmux_popup() {
                    Ok(choice) => choice,
                    // Popup failed (e.g. tmux < 3.2); fzf still works inline.
                    Err(_) => myco::session_browser::pick(None)?,
                }
            } else {
                myco::session_browser::pick(None)?
            };
            match choice {
                Some(id) => Session::load_by_id_or_prefix(&id),
                None => Err(RESUME_CANCELLED.into()),
            }
        }
    }
}

fn print_session_list(list: &[SessionListEntry], ui: &TuiProducer) {
    if list.is_empty() {
        ui.line("(no sessions)");
        return;
    }
    for (i, s) in list.iter().enumerate() {
        ui.line(&format_session_list_line(i + 1, s));
    }
}

// ---------------------------------------------------------------------------
// Transcript helpers (layout lives in myco::session::transcript / myco::tui)
// ---------------------------------------------------------------------------

/// Nuke scrollback + visible screen (same as `clear`), then reprint the whole
/// conversation history so nothing is lost. Triggered by Ctrl-L; the prompt
/// loop reprints the USER header on its next iteration. The cursor codes and
/// the replay are terminal-only: this is a redraw of content the console
/// mirror already holds.
fn clear_and_reprint(agent: &Agent, ui: &TuiProducer) {
    print!("\x1B[3J\x1B[2J\x1B[1;1H");
    let _ = std::io::stdout().flush();
    ui.replay_history(agent.history());
}

// ---------------------------------------------------------------------------
// rustyline: slash-command (+ /resume id) completion
// ---------------------------------------------------------------------------

struct ReplHelper;

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        if !before.starts_with('/') {
            return Ok((0, Vec::new()));
        }

        // `/resume <prefix>` → session ids.
        if let Some(rest) = before.strip_prefix("/resume")
            && rest.starts_with(char::is_whitespace)
        {
            let prefix = rest.trim_start();
            let start = before.len() - prefix.len();
            let pairs = session_id_completions(prefix)
                .into_iter()
                .map(|id| Pair {
                    display: id.clone(),
                    replacement: id,
                })
                .collect();
            return Ok((start, pairs));
        }

        // `/effort <level>` → low|medium|high|max.
        if let Some(rest) = before.strip_prefix("/effort")
            && rest.starts_with(char::is_whitespace)
        {
            let prefix = rest.trim_start().to_ascii_lowercase();
            let start = before.len() - rest.trim_start().len();
            let pairs = ["low", "medium", "high", "max"]
                .into_iter()
                .filter(|level| level.starts_with(prefix.as_str()))
                .map(|level| Pair {
                    display: level.to_string(),
                    replacement: level.to_string(),
                })
                .collect();
            return Ok((start, pairs));
        }

        // Complete slash commands from the start of the line.
        let pairs = SLASH_COMMANDS
            .iter()
            .filter(|c| c.starts_with(before))
            .map(|c| Pair {
                display: (*c).to_string(),
                replacement: (*c).to_string(),
            })
            .collect();
        Ok((0, pairs))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}
impl Highlighter for ReplHelper {}
impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

fn session_id_completions(prefix: &str) -> Vec<String> {
    let prefix = prefix.to_ascii_lowercase();
    let Ok(list) = list_sessions(50) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = list
        .into_iter()
        .map(|s| s.id)
        .filter(|id| id.starts_with(&prefix))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

// ---------------------------------------------------------------------------
/// Sink for `-p/--print`: root-agent answer text streams to stdout verbatim
/// (no sections, colors, or wrapping — the output is meant for pipes).
/// Thinking and tool activity are not rendered.
#[derive(Default)]
struct PrintEventSink {
    /// (wrote anything, ended with newline) — drives the closing newline.
    tail: std::sync::Mutex<(bool, bool)>,
}

impl PrintEventSink {
    /// Terminate the answer with a newline when the model's text didn't.
    fn finish(&self) {
        let (wrote, newline) = *self.tail.lock().unwrap_or_else(|e| e.into_inner());
        if wrote && !newline {
            println!();
        }
        let _ = std::io::stdout().flush();
    }
}

impl EventSink for PrintEventSink {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::TextDelta {
            text,
            context: TraceContext { depth: 0, .. },
        } = event
        {
            if text.is_empty() {
                return;
            }
            print!("{text}");
            let _ = std::io::stdout().flush();
            *self.tail.lock().unwrap_or_else(|e| e.into_inner()) = (true, text.ends_with('\n'));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use myco::generative_model::{Content, Message, ToolResult, ToolUse, TurnEndReason};
    use myco::uuid_simple_hex;
    use serde_json::json;
    use std::time::Duration;

    fn sample_messages() -> Vec<Message> {
        vec![
            Message::UserMessage {
                content: vec![Content::Text {
                    text: "hello".into(),
                }],
            },
            Message::AssistantMessage {
                content: vec![Content::Text {
                    text: "hi there".into(),
                }],
                tool_uses: vec![ToolUse {
                    id: "toolu_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "echo hi"}),
                }],
                turn_end_reason: Some(TurnEndReason::ToolUse),
            },
            Message::ToolResults {
                tool_use_results: vec![ToolResult {
                    id: "toolu_1".into(),
                    content: vec![Content::Text {
                        text: "hi\n".into(),
                    }],
                    is_error: false,
                }],
            },
        ]
    }

    #[test]
    fn session_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "myco-session-test-{}",
            uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess.json");

        let mut session = Session {
            version: myco::SESSION_FILE_VERSION,
            id: "aabbccddeeff00112233445566778899".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            model: "claude-opus-4-8".into(),
            messages: sample_messages(),
            title: Some("roundtrip".into()),
            links: vec![],
            scratchpad: String::new(),
            parent_session_id: None,
            kind: myco::SessionKind::User,
            predecessor_id: None,
            successor_id: None,
            last_usage: None,
        };
        session.updated_at = session.created_at + Duration::from_secs(1);

        let json = serde_json::to_vec_pretty(&session).unwrap();
        fs::write(&path, &json).unwrap();

        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.model, session.model);
        assert_eq!(loaded.title.as_deref(), Some("roundtrip"));
        assert_eq!(loaded.messages.len(), session.messages.len());
        assert_eq!(
            serde_json::to_value(&loaded.messages).unwrap(),
            serde_json::to_value(&session.messages).unwrap()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn message_serde_externally_tagged() {
        let msgs = sample_messages();
        let v = serde_json::to_value(&msgs).unwrap();
        assert!(v[0].get("UserMessage").is_some());
        let back: Vec<Message> = serde_json::from_value(v).unwrap();
        assert_eq!(back.len(), msgs.len());
    }

    #[test]
    fn effort_parses_aliases() {
        assert_eq!("low".parse::<Effort>().unwrap(), Effort::Low);
        assert_eq!("MED".parse::<Effort>().unwrap(), Effort::Medium);
        assert_eq!("h".parse::<Effort>().unwrap(), Effort::High);
        assert_eq!("max".parse::<Effort>().unwrap(), Effort::Max);
        assert!("nope".parse::<Effort>().is_err());
    }

    #[test]
    fn print_flag_parses_bare_and_with_prompt() {
        assert_eq!(Args::parse_from(["myco"]).print, None);
        assert_eq!(Args::parse_from(["myco", "-p"]).print, Some(None));
        assert_eq!(
            Args::parse_from(["myco", "-p", "hello"]).print,
            Some(Some("hello".into()))
        );
        let args = Args::parse_from(["myco", "--print", "hello", "--model", "k"]);
        assert_eq!(args.print, Some(Some("hello".into())));
        assert_eq!(args.model.as_deref(), Some("k"));
        // A following flag is not swallowed as the prompt.
        let args = Args::parse_from(["myco", "-p", "--model", "k"]);
        assert_eq!(args.print, Some(None));
        assert_eq!(args.model.as_deref(), Some("k"));
    }

    #[test]
    fn print_prompt_stdin_is_context_arg_is_instruction() {
        assert_eq!(
            assemble_print_prompt(
                Some("what does this do?".into()),
                Some("fn main() {}\n".into())
            )
            .unwrap(),
            "fn main() {}\n\nwhat does this do?"
        );
        assert_eq!(
            assemble_print_prompt(Some("hi".into()), None).unwrap(),
            "hi"
        );
        assert_eq!(
            assemble_print_prompt(None, Some("just stdin\n".into())).unwrap(),
            "just stdin\n"
        );
    }

    #[test]
    fn print_prompt_requires_some_input() {
        assert!(assemble_print_prompt(None, None).is_err());
        assert!(assemble_print_prompt(Some("  ".into()), Some(" \n".into())).is_err());
    }

    #[test]
    fn print_mentions_parsed_from_arg_only() {
        // A (missing) image path in the piped-stdin portion of the prompt must
        // not attach or error; the same path in the -p argument must error.
        let prompt = "diff mentions @no-such-file.png\n\nreview this".to_string();
        let content = print_turn_content(Some("review this"), prompt.clone()).unwrap();
        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], Content::Text { text } if *text == prompt));
        assert!(print_turn_content(Some("look at @no-such-file.png"), prompt).is_err());
    }

    #[test]
    fn print_arg_mention_attaches_image_before_text() {
        let dir = std::env::temp_dir().join(format!(
            "myco-print-attach-{}",
            uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        fs::create_dir_all(&dir).unwrap();
        let img = dir.join("shot.png");
        fs::write(&img, b"not-really-a-png").unwrap();

        let arg = format!("what is this? @{}", img.display());
        let content = print_turn_content(Some(&arg), arg.clone()).unwrap();
        assert_eq!(content.len(), 2);
        assert!(matches!(&content[0], Content::Image { .. }));
        assert!(matches!(&content[1], Content::Text { text } if *text == arg));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_session_marks_fresh_session_hidden() {
        let args = Args::parse_from(["myco", "-p", "task", "--parent-session", "abc123"]);
        let session = initial_session_or_exit(&args, "some-model");
        assert!(matches!(session.kind, SessionKind::Subagent));
        assert_eq!(session.parent_session_id.as_deref(), Some("abc123"));
        assert!(session.messages.is_empty());
    }

    #[test]
    fn new_session_starts_empty() {
        let session = Session::new("grok-4.5-build");
        assert!(session.messages.is_empty());
    }

    #[test]
    fn input_echo_rows_counts_terminal_character_wrap() {
        assert_eq!(input_echo_rows("", 80), 1);
        assert_eq!(input_echo_rows("short", 80), 1);
        assert_eq!(input_echo_rows(&"x".repeat(80), 80), 1);
        assert_eq!(input_echo_rows(&"x".repeat(81), 80), 2);
        assert_eq!(input_echo_rows("a\nb", 80), 2);
        assert_eq!(input_echo_rows("a\n\nb", 80), 3);
        // CJK columns count double: 41 ideographs = 82 cols.
        assert_eq!(input_echo_rows(&"宽".repeat(41), 80), 2);
    }
}
