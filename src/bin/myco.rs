use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use clap::{CommandFactory, Parser, ValueEnum};
use myco::generative_model::{
    self, BackendConfig, CatalogModel, Content, Effort, GenerativeModelConfig,
};
use myco::host::HostWorker;
use myco::session::{
    ActiveSession, CompactOptions, ConsoleLog, MarkdownRenderer, Palette, RECENT_SESSION_LIMIT,
    Session, SessionListEntry, banner_rule, compact_session, compact_subagent_prompt,
    format_session_detail, format_session_list_line, format_tool_invocation, link_compact_pair,
    list_sessions, print_session_history, render_block, resolve_and_load_session, section_rule,
    user_rule, write_error_section,
};
use myco::{
    Agent, AgentEvent, ColorMode, Config, ConfigUserSettings, EventSink, Harness, NullEventSink,
    SessionHistoryTool, SessionKind, SessionMetaTool, StartupPreflight, TraceContext, WrapMode,
    prompts, uuid_simple_hex,
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

    /// Write the picked session id to FILE instead of stdout. Only with
    /// `--mode session-browser`; used by the bare-/resume tmux popup handshake.
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// With `--mode session-browser`: rank sessions matching QUERY (keyword
    /// search over title/first message/scratchpad/console tail, semantic
    /// fallback) instead of listing by recency.
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
    match args.mode {
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
    // Owner request: index skills / AGENTS.md under the worker's cwd.
    // Construction alone never indexes (tests rely on that).
    let search = myco::TextSearchToolService::new();
    if let Ok(cwd) = std::env::current_dir() {
        search.auto_index_under(cwd);
    }
    if let Err(e) = HostWorker::standard_with_search(args.name, search)
        .serve_stdio()
        .await
    {
        eprintln!("myco host error: {e}");
        std::process::exit(1);
    }
}

async fn run_interactive(args: Args) {
    // One explicit resolution step: model catalog (gateways/models + auth),
    // harness hosts and default model key (--config → $MYCO_CONFIG →
    // ~/.myco/config.toml), and the color decision. Everything downstream
    // reads this, not the env or config files.
    let app_config = Config::resolve(ConfigUserSettings {
        harness_config_path: args.config.clone(),
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
            eprintln!("config: {}", app_config.harness_config_path.display());
            std::process::exit(2);
        }
    };
    let model_key = catalog_model.spec.key.clone();
    let palette = Palette::colored(app_config.colors_enabled)
        .with_wrap(effective_wrap_width(app_config.wrap_max));

    // Startup preflight: verify expected executables resolve (bash, lynx;
    // OpenSSH tools when remotes are configured), then unlock SSH identities
    // via the existing ssh-agent before attach — remote hosts use
    // `ssh -o BatchMode=yes` (NDJSON pipe is not a TTY), so OpenSSH must never
    // need to prompt on the host pipe.
    // Problems are printed after the banner (WARNING block), not here.
    let preflight = StartupPreflight::run(&app_config.harness.remote_hosts);

    // Session handle first so `session_meta` can share it with the agent harness.
    let resuming = args.resume.is_some();
    if resuming && args.parent_session.is_some() {
        // A resumed session already carries its kind/parent; rewriting them
        // here would silently change a stored session's identity.
        eprintln!("--parent-session applies only to a fresh session; drop it when resuming");
        std::process::exit(2);
    }
    let initial_session = match args.resume {
        Some(id_opt) => load_resume_session_or_exit(id_opt.as_deref()),
        None => {
            let mut fresh = Session::new(model_key.clone());
            if let Some(parent) = args.parent_session.as_deref() {
                let parent = parent.trim();
                if parent.is_empty() {
                    eprintln!("--parent-session needs a non-empty session id");
                    std::process::exit(2);
                }
                fresh.kind = SessionKind::Subagent;
                fresh.parent_session_id = Some(parent.to_string());
            }
            fresh
        }
    };
    let active_session = ActiveSession::new(initial_session);

    // Plain-text console mirror ({id}.console), TTY-gated like colors/wrap. It
    // resolves the current session id per append, so /new, /compact, /resume
    // redirect it automatically. The agent can read it to see exactly what the
    // user saw, including live-only WARNING/ERROR sections.
    let console = Arc::new(ConsoleLog::new(
        active_session.clone(),
        app_config.stdout_is_tty,
    ));

    let session_tool =
        Arc::new(SessionMetaTool::new(active_session.clone())) as Arc<dyn myco::ToolService>;
    let history_tool = Arc::new(SessionHistoryTool::new()) as Arc<dyn myco::ToolService>;
    let harness = Harness::attach_with_root_services(
        app_config.harness.clone(),
        vec![session_tool, history_tool],
    )
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
        eprintln!("config: {}", app_config.harness_config_path.display());
        std::process::exit(1);
    });
    // Owner request: index skills / AGENTS.md under the launch directory.
    // Attach alone never indexes (tests rely on that).
    if let Ok(cwd) = std::env::current_dir() {
        harness.auto_index_local(cwd);
    }
    // Thinking/reasoning is always requested; UI shows summary lines only (not stored).
    let mut effort = args.effort;
    let debug_dump_api_requests = args.debug_dump_api_requests;
    let model = build_model(&catalog_model, &harness, debug_dump_api_requests, effort);
    let sink = Arc::new(CliEventSink::new(palette, console.clone()));
    let mut agent = Agent::new(model, harness.clone(), sink.clone());
    agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
    let restored = active_session.snapshot();
    agent.set_history(restored.messages.clone());
    agent.set_last_usage(restored.last_usage);
    let ctrl_l = Arc::new(AtomicBool::new(false));
    let mut editor = build_editor(ctrl_l.clone());

    load_readline_history(&mut editor, &active_session);

    let session_label = active_session.with(|s| match &s.title {
        Some(t) if !t.is_empty() => format!("{} \"{t}\"", s.id),
        _ => s.id.clone(),
    });
    // Startup chrome: headed banner block (rule, MYCO, model/session, key
    // hints). Hosts via /hosts, effort via /effort, config path via
    // attach-failure hints. Rendered to a buffer so the console mirror gets
    // the same bytes (stripped) the terminal does.
    let mut chrome = Vec::new();
    let _ = write_startup_banner(&mut chrome, &model_key, &session_label, palette);
    // Preflight problems (missing executables, ssh-agent) open one WARNING
    // block after the banner, before the first USER block; happy path silent.
    let _ = preflight.write_warning_section(&mut chrome, palette);
    // Blank line closes the startup chrome before the first USER rule
    // (or the resumed-history replay).
    chrome.push(b'\n');
    emit_mirrored(&console, &chrome);
    // The resumed-history replay is not mirrored: {id}.console already holds it
    // from the run(s) that produced it (the file is opened for append).
    if resuming {
        print_session_history(agent.history(), palette);
    }

    run_repl(
        &mut agent,
        &active_session,
        &mut editor,
        harness.clone(),
        &catalog_model,
        &mut effort,
        debug_dump_api_requests,
        ctrl_l,
        palette,
        app_config.wrap_max,
        app_config.repaint_enabled,
        sink,
        console,
    )
    .await;

    if let Err(e) = persist_session(&agent, &active_session, /*force*/ true) {
        eprintln!("warning: could not save session on exit: {e}");
    }
    if let Err(e) = save_readline_history(&mut editor, &active_session) {
        eprintln!("warning: could not save history on exit: {e}");
    }
    // Only announce a session id if we actually wrote one (non-empty history).
    if !agent.history().is_empty() || active_session.snapshot().json_path().exists() {
        println!("session={}", active_session.id());
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

/// Write the startup banner: full-block rule, MYCO title, model/session lines, and
/// the two hints worth surfacing before the first prompt (/help, newline chord).
fn write_startup_banner(
    out: &mut impl Write,
    model_key: &str,
    session_label: &str,
    palette: Palette,
) -> std::io::Result<()> {
    writeln!(out, "{}", palette.banner(&banner_rule(palette.wrap)))?;
    writeln!(out, "{}", palette.banner("MYCO"))?;
    writeln!(out)?;
    writeln!(out, "Model: {model_key}")?;
    writeln!(out, "Session: {session_label}")?;
    writeln!(out)?;
    writeln!(out, "/help for commands")?;
    writeln!(out)?;
    writeln!(out, "Alt-Enter or Ctrl-J for newline")?;
    Ok(())
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

#[allow(clippy::too_many_arguments)]
async fn run_repl(
    agent: &mut Agent,
    session: &ActiveSession,
    editor: &mut Editor<ReplHelper, DefaultHistory>,
    harness: Arc<Harness>,
    catalog_model: &CatalogModel,
    effort: &mut Effort,
    debug_dump_api_requests: bool,
    ctrl_l: Arc<AtomicBool>,
    mut palette: Palette,
    wrap_max: Option<usize>,
    repaint: bool,
    sink: Arc<CliEventSink>,
    console: Arc<ConsoleLog>,
) {
    let mut last_wrap = palette.wrap;
    loop {
        // Re-measure the terminal each prompt: after a resize, reflow the
        // whole dialog at the new width (same clear+reprint as Ctrl-L). This
        // is the safe point — never mid-stream, never while rustyline owns
        // the terminal. Dumb terminals skip the reprint (no cursor codes)
        // but still pick up the new width for subsequent turns. The reflow is
        // a redraw, not new content, so it is not mirrored to the console.
        let wrap = effective_wrap_width(wrap_max);
        if wrap != last_wrap {
            last_wrap = wrap;
            palette = palette.with_wrap(wrap);
            sink.set_wrap(wrap);
            if repaint && !agent.history().is_empty() {
                clear_and_reprint(agent, palette);
            }
        }
        let max = agent.context_window_tokens();
        let usage = agent.last_usage();
        // `?` = resumed before usage was tracked; `0` = genuinely empty session.
        let used = match usage {
            Some(u) => u.context_tokens().to_string(),
            None if agent.history().is_empty() => "0".to_string(),
            None => "?".to_string(),
        };
        let mut header = Vec::new();
        let _ = writeln!(header, "{}", palette.user(&user_rule(palette.wrap)));
        let _ = writeln!(header, "{}", palette.user(&format!("USER {used}/{max}")));
        if let Some(u) = usage {
            let _ = writeln!(
                header,
                "{}",
                palette.user(&format!(
                    "input {}  output {}  cached input {}  cached output {}",
                    u.input_tokens, u.output_tokens, u.cached_input_tokens, u.cached_output_tokens
                ))
            );
        }
        for line in harness.running_tool_summaries(agent.context().agent_id) {
            let _ = writeln!(header, "{}", palette.user(&line));
        }
        let _ = writeln!(header);
        emit_mirrored(&console, &header);
        // No "> " prefix; body is typed on the line after the USER header.
        // Multiline: Alt-Enter / Ctrl-J inserts a newline in-buffer; plain Enter
        // submits the whole buffer to the agent.
        let line = match editor.readline("") {
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
        if ctrl_l.swap(false, Ordering::SeqCst) {
            clear_and_reprint(agent, palette);
            continue;
        }

        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        reprint_input_wrapped(&line, palette, repaint);
        // Mirror the submitted line once (wrap-only, as shown). rustyline echoed
        // it to the terminal but not to us; the re-echo above only moves the
        // cursor, so the console needs the logical text here.
        console.append(&render_block(
            &line,
            Palette::plain().with_wrap(palette.wrap),
        ));
        console.append("\n");
        if is_exit_command(&input) {
            break;
        }
        if let Some(cmd) = parse_meta(&input) {
            if matches!(cmd, MetaCommand::Compact) {
                run_compact(
                    agent,
                    session,
                    editor,
                    harness.clone(),
                    catalog_model,
                    palette,
                )
                .await;
                continue;
            }
            handle_meta(
                cmd,
                agent,
                session,
                editor,
                harness.as_ref(),
                catalog_model,
                effort,
                debug_dump_api_requests,
                palette,
            );
            continue;
        }

        run_user_turn(agent, session, editor, input, palette, &console).await;
    }
}

/// Write rendered bytes to stdout and to the console mirror (the mirror strips
/// ANSI). Used for buffered blocks (banner, preflight, ERROR); the streaming
/// sink and REPL headers mirror their own `print!`s directly.
fn emit_mirrored(console: &ConsoleLog, rendered: &[u8]) {
    let mut out = std::io::stdout();
    let _ = out.write_all(rendered);
    let _ = out.flush();
    console.append(&String::from_utf8_lossy(rendered));
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
fn reprint_input_wrapped(line: &str, palette: Palette, repaint: bool) {
    if palette.wrap.is_none() || !repaint {
        return;
    }
    let Some((cols, screen_rows)) = myco::config::detect_terminal_size() else {
        return;
    };
    let wrapped = render_block(line, Palette::plain().with_wrap(palette.wrap));
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

async fn run_user_turn(
    agent: &mut Agent,
    session: &ActiveSession,
    editor: &mut Editor<ReplHelper, DefaultHistory>,
    input: String,
    palette: Palette,
    console: &ConsoleLog,
) {
    let _ = editor.add_history_entry(&input);
    if let Err(e) = save_readline_history(editor, session) {
        eprintln!("warning: could not save history: {e}");
    }
    if let Err(e) = session.maybe_auto_title_from_user_text(&input) {
        eprintln!("warning: could not auto-title session: {e}");
    }

    // Cancel in-flight turn on Ctrl-C while interact runs. At the prompt, rustyline
    // still owns Ctrl-C (clears the line). We only install a SIGINT bridge here.
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

    // First assistant section opens with its own blank line + thin rule + header.
    match agent
        .interact(vec![Content::Text { text: input }], cancel)
        .await
    {
        Ok(_) => {
            println!();
            console.append("\n");
        }
        Err(myco::AgentInteractionError::Cancelled) => {
            println!();
            println!("(cancelled)");
            console.append("\n(cancelled)\n");
        }
        Err(e) => {
            // Close any open ASSISTANT stream state and show a headed ERROR section.
            // Generate failures (context overflow, provider errors) are live-only —
            // not stored in session history — so resume/Ctrl-L will not replay them.
            // Buffered so the console mirror captures this live-only section too.
            let mut block = Vec::new();
            let _ = write_error_section(&mut block, &e.to_string(), palette);
            emit_mirrored(console, &block);
            println!();
            console.append("\n");
        }
    }

    sigint_task.abort();

    // Persist whatever history the agent has, including failed/cancelled turns.
    if let Err(e) = persist_session(agent, session, /*force*/ true) {
        eprintln!("warning: could not save session: {e}");
    }
    if let Err(e) = save_readline_history(editor, session) {
        eprintln!("warning: could not save history: {e}");
    }
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

async fn run_compact(
    agent: &mut Agent,
    session: &ActiveSession,
    editor: &mut Editor<ReplHelper, DefaultHistory>,
    harness: Arc<Harness>,
    catalog_model: &CatalogModel,
    palette: Palette,
) {
    if let Err(e) = session.persist_messages(agent.history(), agent.last_usage(), true) {
        eprintln!("compact: failed to persist current session: {e}");
        return;
    }
    let predecessor = session.snapshot();
    if predecessor.messages.is_empty() {
        eprintln!("compact: session is empty");
        return;
    }

    println!("compacting session={} …", predecessor.id);

    let worker_id = uuid::Uuid::new_v4();
    let worker_hex = uuid_simple_hex(worker_id);
    let mut worker_session = Session::new_hidden(
        catalog_model.spec.key.clone(),
        worker_hex.clone(),
        SessionKind::Compact,
        Some(predecessor.id.clone()),
    );
    worker_session.title = Some(format!(
        "compact {}",
        &predecessor.id[..8.min(predecessor.id.len())]
    ));
    if let Err(e) = worker_session.save() {
        eprintln!("warning: could not save compact worker session: {e}");
    }

    let model = match generative_model::new(GenerativeModelConfig {
        model: catalog_model.spec.clone(),
        tools: harness.tool_specs(),
        system_prompt: [
            "You are a myco compaction worker. Follow the user instruction exactly. \
             Prefer session_history over bash for reading sessions."
                .to_string(),
            prompts::agent_prompt_epilogue(),
        ]
        .join("\n\n"),
        backend_config: catalog_model.backend.clone(),
    }) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("compact: failed to create model: {e:?}");
            return;
        }
    };

    let sink = Arc::new(NullEventSink);
    let mut worker = Agent::with_context(
        model,
        harness.clone(),
        sink,
        TraceContext {
            agent_id: worker_id,
            depth: 1,
            parent_tool_use_id: None,
        },
    );
    worker.set_context_window_tokens(catalog_model.spec.context_window_tokens);

    let prompt = compact_subagent_prompt(&predecessor.id);
    let cancel = myco::CancelToken::new();
    let result = worker
        .interact(vec![Content::Text { text: prompt }], cancel)
        .await;

    worker_session.messages = worker.history().to_vec();
    worker_session.touch();
    let _ = worker_session.save();

    match result {
        Ok(_) => {}
        Err(e) => {
            eprintln!("compact: worker failed: {e}");
            return;
        }
    }

    let summary_path = predecessor.summary_path();
    let summary = match std::fs::read_to_string(&summary_path) {
        Ok(s) if !s.trim().is_empty() => s,
        Ok(_) => {
            eprintln!(
                "compact: worker finished but summary file is empty ({})",
                summary_path.display()
            );
            return;
        }
        Err(e) => {
            eprintln!(
                "compact: worker finished but summary missing at {}: {e}",
                summary_path.display()
            );
            return;
        }
    };

    let (successor, outcome) = match compact_session(
        &predecessor,
        &summary,
        &catalog_model.spec.key,
        &CompactOptions::default(),
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("compact: failed to build successor: {e}");
            return;
        }
    };

    let mut pred = predecessor;
    if let Err(e) = link_compact_pair(&mut pred, &successor) {
        eprintln!("compact: failed to link sessions: {e}");
        return;
    }

    // Switch live REPL to successor.
    if let Err(e) = save_readline_history(editor, session) {
        eprintln!("warning: could not save history: {e}");
    }
    session.replace(successor.clone());
    agent.set_history(successor.messages.clone());
    agent.set_last_usage(successor.last_usage);
    reload_readline_history(editor, session);
    clear_and_reprint(agent, palette);
    println!(
        "compacted → new session={}  from={}  kept_tail={} messages  summary={}",
        outcome.successor_id,
        outcome.predecessor_id,
        outcome.tail_messages,
        outcome.summary_path.display()
    );
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

#[allow(clippy::too_many_arguments)]
fn handle_meta(
    cmd: MetaCommand<'_>,
    agent: &mut Agent,
    session: &ActiveSession,
    editor: &mut Editor<ReplHelper, DefaultHistory>,
    harness: &Harness,
    catalog_model: &CatalogModel,
    effort: &mut Effort,
    debug_dump_api_requests: bool,
    palette: Palette,
) {
    match cmd {
        MetaCommand::Help => print_help(),
        MetaCommand::Session => {
            let _ = session.persist_messages(agent.history(), agent.last_usage(), false);
            print!("{}", format_session_detail(&session.snapshot()));
        }
        MetaCommand::Sessions => match list_sessions(0) {
            Ok(list) => {
                let shown = RECENT_SESSION_LIMIT.min(list.len());
                print_session_list(&list[..shown]);
                if list.len() > shown {
                    println!(
                        "  … {} more — bare /resume opens the session browser",
                        list.len() - shown
                    );
                }
            }
            Err(e) => eprintln!("Failed to list sessions: {e}"),
        },
        MetaCommand::Hosts => print_host_status(harness),
        MetaCommand::New => {
            save_before_switch(agent, session, editor);
            // A nested run stays nested across /new: carry kind + parent lineage.
            let snapshot = session.snapshot();
            let mut fresh = Session::new(catalog_model.spec.key.clone());
            fresh.kind = snapshot.kind;
            fresh.parent_session_id = snapshot.parent_session_id.clone();
            session.replace(fresh);
            agent.set_history(Vec::new());
            agent.set_last_usage(None);
            reload_readline_history(editor, session);
            // Fresh canvas for a fresh session (same clear as Ctrl-L, empty history).
            clear_and_reprint(agent, palette);
            println!("new session={}", session.id());
        }
        MetaCommand::Resume(arg) => {
            save_before_switch(agent, session, editor);
            match resolve_resume_session(arg) {
                Ok(loaded) => {
                    install_session(agent, editor, session, &loaded);
                    println!(
                        "resumed session={}  messages={}",
                        session.id(),
                        agent.history().len()
                    );
                    print_session_history(agent.history(), palette);
                }
                Err(e) if e == RESUME_CANCELLED => println!("resume cancelled"),
                Err(e) => eprintln!("resume failed: {e}"),
            }
        }
        MetaCommand::Effort(arg) => match arg {
            None => println!("effort={effort}  (low|medium|high|max)"),
            Some(s) => match s.parse::<Effort>() {
                Ok(next) if next == *effort => println!("effort={effort}  (unchanged)"),
                Ok(next) => {
                    *effort = next;
                    let model =
                        build_model(catalog_model, harness, debug_dump_api_requests, *effort);
                    agent.set_model(model);
                    agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
                    println!("effort={effort}");
                }
                Err(e) => eprintln!("{e}"),
            },
        },
        MetaCommand::Title(arg) => match arg {
            None => {
                let snap = session.snapshot();
                match snap.title.as_deref() {
                    Some(t) if !t.is_empty() => println!("title={t:?}"),
                    _ => println!("title=(none)"),
                }
            }
            Some(t) if t.trim().is_empty() => {
                if let Err(e) = session.with_mut(|s| {
                    s.set_title(None)?;
                    s.touch();
                    s.save()
                }) {
                    eprintln!("failed to clear title: {e}");
                } else {
                    println!("title=(none)");
                }
            }
            Some(t) => {
                if let Err(e) = session.with_mut(|s| {
                    s.set_title(Some(t.to_string()))?;
                    s.touch();
                    s.save()
                }) {
                    eprintln!("failed to set title: {e}");
                } else if let Some(title) = session.snapshot().title {
                    println!("title={title:?}");
                }
            }
        },
        MetaCommand::Compact => {
            // Handled asynchronously in run_repl.
        }
    }
}

fn save_before_switch(
    agent: &Agent,
    session: &ActiveSession,
    editor: &mut Editor<ReplHelper, DefaultHistory>,
) {
    if let Err(e) = persist_session(agent, session, /*force*/ false) {
        eprintln!("warning: could not save current session: {e}");
    }
    if let Err(e) = save_readline_history(editor, session) {
        eprintln!("warning: could not save history: {e}");
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

fn print_help() {
    println!(
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

Thinking/reasoning is always requested (default effort=high). The UI shows a
`Thinking: …` summary inside ASSISTANT; it is stored in session history for
resume but stripped from provider requests. Change effort with `/effort`.
Generate failures open a headed ERROR section (live only; not in history).

Each USER header shows `USER <used>/<max>` context tokens when the provider
reported usage on the previous generate (0/max until then). Below the token
counts, one line per still-running tool (live bash session on the in-process
local host) shows its command, uptime, and idle time.

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
cancelled results for every tool_use."
    );
}

fn print_host_status(harness: &Harness) {
    let statuses = harness.host_status();
    if statuses.is_empty() {
        println!("hosts: (none)");
        return;
    }
    println!(
        "hosts: default=local  ({} total; local always in-process)",
        statuses.len()
    );
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
            Some(err) => println!("  [{state}] {}  tools={tools}{cmd}  err={err}", s.name),
            None => println!("  [{state}] {}  tools={tools}{cmd}", s.name),
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
    if history.is_empty()
        && session.snapshot().messages.is_empty()
        && !session.snapshot().json_path().exists()
    {
        return Ok(());
    }
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

fn install_session(
    agent: &mut Agent,
    editor: &mut Editor<ReplHelper, DefaultHistory>,
    active: &ActiveSession,
    session: &Session,
) {
    active.replace(session.clone());
    agent.set_history(session.messages.clone());
    agent.set_last_usage(session.last_usage);
    reload_readline_history(editor, active);
}

fn print_session_list(list: &[SessionListEntry]) {
    if list.is_empty() {
        println!("(no sessions)");
        return;
    }
    for (i, s) in list.iter().enumerate() {
        println!("{}", format_session_list_line(i + 1, s));
    }
}

// ---------------------------------------------------------------------------
// Transcript helpers (layout lives in myco::session::transcript)
// ---------------------------------------------------------------------------

/// Nuke scrollback + visible screen (same as `clear`), then reprint the whole
/// conversation history so nothing is lost. Triggered by Ctrl-L; the prompt
/// loop reprints the USER header on its next iteration.
fn clear_and_reprint(agent: &Agent, palette: Palette) {
    print!("\x1B[3J\x1B[2J\x1B[1;1H");
    let _ = std::io::stdout().flush();
    print_session_history(agent.history(), palette);
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
// Event sink — sectioned live rendering
// ---------------------------------------------------------------------------

/// Live stdout rendering for the root agent.
///
/// Headed sections: USER (printed by the REPL), ASSISTANT (this sink), and
/// ERROR (printed by the REPL on generate failure). Thinking summaries, tool
/// invocations, and answer text are paragraphs inside a single ASSISTANT
/// section for the whole agent turn (including multi-step tool loops).
/// Thinking is also stored in session history for resume (backends strip it
/// on the next API request). ERROR sections are live-only and not replayed.
///
/// Opening an ASSISTANT section:
/// blank line + thin `SECTION_RULE` + `ASSISTANT` + blank line, then body.
struct CliEventSink {
    state: Mutex<SinkState>,
    /// Behind a lock so the REPL can update the wrap width after a terminal
    /// resize; renderers created afterwards pick it up.
    palette: Mutex<Palette>,
    /// Plain-text mirror: every byte this sink prints is also appended here
    /// (ANSI stripped) so the agent can read the exact ASSISTANT transcript.
    console: Arc<ConsoleLog>,
}

struct SinkState {
    at_line_start: bool,
    /// Whether the ASSISTANT header is already open for this agent turn.
    assistant_open: bool,
    /// True after a finished paragraph so the next one gets a blank line.
    need_blank: bool,
    /// True while streaming answer text (no blank lines between text deltas).
    in_text_stream: bool,
    /// Streaming markdown/wrap renderer for the current answer-text paragraph.
    text_md: Option<MarkdownRenderer>,
    /// Live thinking-summary line builder (UI only).
    thinking_line_open: bool,
    thinking_buf: String,
    thinking_md: Option<MarkdownRenderer>,
}

impl CliEventSink {
    fn new(palette: Palette, console: Arc<ConsoleLog>) -> Self {
        Self {
            state: Mutex::new(SinkState {
                at_line_start: true,
                assistant_open: false,
                need_blank: false,
                in_text_stream: false,
                text_md: None,
                thinking_line_open: false,
                thinking_buf: String::new(),
                thinking_md: None,
            }),
            palette: Mutex::new(palette),
            console,
        }
    }

    fn palette(&self) -> Palette {
        *self.palette.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `print!` to stdout and mirror the same bytes to the console log.
    fn out(&self, s: &str) {
        print!("{s}");
        self.console.append(s);
    }

    /// `println!` to stdout and mirror the line (with its newline) to the log.
    fn outln(&self, s: &str) {
        println!("{s}");
        self.console.append(s);
        self.console.append("\n");
    }

    /// Bare `println!()` mirrored as a newline.
    fn outln_blank(&self) {
        println!();
        self.console.append("\n");
    }

    /// Update the wrap width mid-session (terminal resize). An in-flight
    /// paragraph keeps the width its renderer was created with.
    fn set_wrap(&self, wrap: Option<usize>) {
        let mut palette = self.palette.lock().unwrap_or_else(|e| e.into_inner());
        *palette = palette.with_wrap(wrap);
    }
}

impl CliEventSink {
    fn with_state<R>(&self, f: impl FnOnce(&mut SinkState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut state)
    }

    fn ensure_line_start(&self) {
        let need_nl = self.with_state(|s| {
            let need = !s.at_line_start;
            s.at_line_start = true;
            need
        });
        if need_nl {
            self.outln_blank();
        }
    }

    /// Open ASSISTANT once per agent turn (multi-step tool loops stay in one section).
    fn ensure_assistant(&self) {
        let need_open = self.with_state(|s| !s.assistant_open);
        if !need_open {
            return;
        }
        self.finish_thinking_line();
        self.ensure_line_start();
        self.outln_blank();
        let palette = self.palette();
        self.outln(&palette.assistant(&section_rule(palette.wrap)));
        self.outln(&palette.assistant("ASSISTANT"));
        self.outln_blank();
        self.with_state(|s| {
            s.at_line_start = true;
            s.assistant_open = true;
            s.need_blank = false;
        });
        let _ = std::io::stdout().flush();
    }

    /// Blank line before a subsequent paragraph inside ASSISTANT.
    fn separate_paragraph_if_needed(&self) {
        let need_blank = self.with_state(|s| s.need_blank);
        if need_blank {
            self.ensure_line_start();
            self.outln_blank();
            self.with_state(|s| s.at_line_start = true);
        }
    }

    fn note_paragraph(&self) {
        self.with_state(|s| s.need_blank = true);
    }

    /// Finish a live `Thinking: …` line with a trailing newline if one is open.
    fn finish_thinking_line(&self) {
        let tail = self.with_state(|s| {
            if !s.thinking_line_open {
                return None;
            }
            s.thinking_line_open = false;
            s.thinking_buf.clear();
            s.at_line_start = true;
            s.in_text_stream = false;
            // Thinking is a finished paragraph for spacing purposes.
            s.need_blank = true;
            Some(
                s.thinking_md
                    .take()
                    .map(|mut r| r.finish())
                    .unwrap_or_default(),
            )
        });
        if let Some(tail) = tail {
            // Flush the renderer and close the dim thinking style it opened.
            self.outln(&tail);
            let _ = std::io::stdout().flush();
        }
    }

    /// Close the current answer-text stream: flush its renderer, mark the
    /// paragraph finished.
    fn end_text_stream(&self) {
        let tail = self.with_state(|s| {
            if !s.in_text_stream {
                return None;
            }
            s.in_text_stream = false;
            s.need_blank = true;
            Some(s.text_md.take().map(|mut r| r.finish()).unwrap_or_default())
        });
        if let Some(tail) = tail
            && !tail.is_empty()
        {
            self.out(&tail);
            self.with_state(|s| s.at_line_start = tail.ends_with('\n'));
            let _ = std::io::stdout().flush();
        }
    }
}

impl EventSink for CliEventSink {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::ThinkingDelta {
                text,
                context: TraceContext { depth: 0, .. },
            } => {
                if text.is_empty() {
                    return;
                }
                // Always show thinking summaries inside ASSISTANT as `Thinking: …`.
                self.ensure_assistant();
                let starting = self.with_state(|s| !s.thinking_line_open);
                if starting {
                    // End answer-text stream so thinking is its own paragraph.
                    self.end_text_stream();
                    self.separate_paragraph_if_needed();
                    self.ensure_line_start();
                    let palette = self.palette();
                    let rendered = self.with_state(|s| {
                        s.thinking_line_open = true;
                        s.thinking_buf = text.clone();
                        // Dim base stays open across deltas; finish_thinking_line resets.
                        let r = s
                            .thinking_md
                            .insert(MarkdownRenderer::with_base(palette, "2"));
                        let mut out = r.feed("Thinking: ");
                        out.push_str(&r.feed(&text));
                        out
                    });
                    self.out(&rendered);
                } else {
                    let rendered = self.with_state(|s| {
                        s.thinking_buf.push_str(&text);
                        s.thinking_md
                            .as_mut()
                            .map(|r| r.feed(&text))
                            .unwrap_or_default()
                    });
                    self.out(&rendered);
                }
                let _ = std::io::stdout().flush();
                self.with_state(|s| {
                    s.at_line_start = s
                        .thinking_md
                        .as_ref()
                        .is_some_and(|r| r.ends_at_line_start());
                });
            }
            AgentEvent::TextDelta {
                text,
                context: TraceContext { depth: 0, .. },
            } => {
                if text.is_empty() {
                    return;
                }
                self.finish_thinking_line();
                self.ensure_assistant();
                // Blank-separate only when starting a new text paragraph after
                // thinking/tools — never between chunks of the same stream.
                let start_new = self.with_state(|s| !s.in_text_stream && s.need_blank);
                if start_new {
                    self.separate_paragraph_if_needed();
                }
                let palette = self.palette();
                let rendered = self.with_state(|s| {
                    s.in_text_stream = true;
                    s.need_blank = false;
                    s.text_md
                        .get_or_insert_with(|| MarkdownRenderer::new(palette))
                        .feed(&text)
                });
                self.out(&rendered);
                let _ = std::io::stdout().flush();
                self.with_state(|s| {
                    s.at_line_start = s.text_md.as_ref().is_some_and(|r| r.ends_at_line_start());
                });
            }
            AgentEvent::TurnFinished {
                context: TraceContext { depth: 0, .. },
                ..
            } => {
                self.finish_thinking_line();
                self.end_text_stream();
                self.ensure_line_start();
                // Close ASSISTANT for the next user turn (REPL prints USER next).
                self.with_state(|s| {
                    s.assistant_open = false;
                    s.need_blank = false;
                    s.in_text_stream = false;
                });
            }
            // Root agent only — hide nested worker noise (depth>0, e.g. compact).
            AgentEvent::ToolStarted {
                tool_use,
                context: TraceContext { depth: 0, .. },
            } => {
                // End any open text/thinking stream so the tool is its own paragraph.
                self.finish_thinking_line();
                self.end_text_stream();
                self.ensure_assistant();
                self.separate_paragraph_if_needed();
                self.out(&format_tool_invocation(
                    &tool_use.name,
                    &tool_use.input,
                    self.palette(),
                ));
                self.with_state(|s| {
                    s.at_line_start = true;
                    s.in_text_stream = false;
                });
                self.note_paragraph();
                let _ = std::io::stdout().flush();
            }
            _ => {}
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
    fn startup_banner_layout() {
        let mut buf = Vec::new();
        write_startup_banner(
            &mut buf,
            "hy3-free",
            "993d14889c414aab81963843cccf8090 \"greeting\"",
            Palette::plain(),
        )
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        let expected = format!(
            "{rule}\nMYCO\n\nModel: hy3-free\n\
             Session: 993d14889c414aab81963843cccf8090 \"greeting\"\n\n\
             /help for commands\n\nAlt-Enter or Ctrl-J for newline\n",
            rule = banner_rule(None)
        );
        assert_eq!(text, expected);
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
