//! The interactive CLI, rebuilt on the session runtime: `myco` spawns a
//! [`Server`] in-process, attaches one live session, and drives it
//! through the same queue and event feed the web server uses.
//!
//! The v1 REPL was synchronous — one turn at a time, the prompt gone while
//! the agent worked. This CLI is **async**: the prompt is always live
//! (rustyline), agent output streams above it through an external printer,
//! and lines typed while a turn runs queue as the next turns. Ctrl-C cancels
//! the in-flight turn; `/exit` leaves (the session stays on disk).

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use rustyline::ExternalPrinter;
use rustyline::error::ReadlineError;

use myco_config::{Config, ConfigUserSettings};
use myco_machines::harness::StartupPreflight;
use myco_session::{Session, format_session_detail, format_session_list_line, list_sessions};

use crate::server::{Cmd, Live, Server, SessionEvent, last_answer};
use crate::tui::{ConsoleTuiSink, StdoutTuiSink, TuiProducer, TuiSink, encode_ansi};
use myco_session::ConsoleLog;

const DEFAULT_WRAP_WIDTH: usize = 80;

pub struct CliOptions {
    pub config_path: Option<std::path::PathBuf>,
    pub model: Option<String>,
    /// `Some(None)` = resume most recent; `Some(Some(id))` = resume that id.
    pub resume: Option<Option<String>>,
    pub parent_session: Option<String>,
    pub fork: bool,
    /// `-p`: one non-interactive turn, answer to stdout, exit.
    pub print: Option<String>,
}

/// Terminal sink that prints **above** the live rustyline prompt, so agent
/// output and the input line coexist — the visible half of the async CLI.
struct PrinterSink<P: ExternalPrinter + Send> {
    printer: Mutex<P>,
    colors: bool,
}

impl<P: ExternalPrinter + Send + Sync> TuiSink for PrinterSink<P> {
    fn emit(&self, events: &[crate::tui::TuiEvent]) {
        let text = encode_ansi(events, self.colors);
        if let Ok(mut p) = self.printer.lock() {
            let _ = p.print(text);
        }
    }
}

pub async fn run(opts: CliOptions) {
    let config = match Config::resolve(ConfigUserSettings {
        config_path: opts.config_path.clone(),
        model: opts.model.clone(),
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("myco: {e}");
            std::process::exit(1);
        }
    };
    let preflight = StartupPreflight::run(&config.harness.remote_hosts, config.max_soul_bytes);
    let default_model = config.model.clone();
    let sup = Server::new(config);

    // Initial session: resume, nested child, or fresh.
    let initial = match (&opts.resume, &opts.parent_session) {
        (Some(id), _) => {
            let id = match id {
                Some(id) => id.clone(),
                None => match list_sessions(1) {
                    Ok(entries) if !entries.is_empty() => entries[0].id.clone(),
                    _ => {
                        eprintln!("myco: no session to resume");
                        std::process::exit(1);
                    }
                },
            };
            match sup.ensure_live(&id, None).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("myco: {e}");
                    std::process::exit(1);
                }
            }
        }
        (None, Some(parent)) => {
            let fresh = match sup.new_child(parent, opts.model.clone(), opts.fork) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("myco: {e}");
                    std::process::exit(1);
                }
            };
            boot_fresh(&sup, fresh).await
        }
        (None, None) => boot_fresh(&sup, Session::new(default_model.clone())).await,
    };

    if let Some(prompt) = opts.print {
        run_print(initial, prompt).await;
        return;
    }

    let tty = std::io::stdout().is_terminal();
    let colors = tty
        && std::env::var("NO_COLOR").is_err()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
    let wrap = tty.then_some(DEFAULT_WRAP_WIDTH);

    // Line editor on its own thread; an external printer carries agent
    // output above the prompt.
    let mut editor = match rustyline::DefaultEditor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("myco: readline: {e}");
            std::process::exit(1);
        }
    };
    let terminal: Arc<dyn TuiSink> = match editor.create_external_printer() {
        Ok(p) => Arc::new(PrinterSink {
            printer: Mutex::new(p),
            colors,
        }),
        Err(_) => Arc::new(StdoutTuiSink { colors }),
    };

    let mut live = initial;
    // Turns queued on the *current* session this run; exit waits for them.
    let mut submitted: u64 = *live.turns.borrow();
    let mut producer = make_producer(&live, terminal.clone(), colors, wrap);
    banner(&producer, &sup, &live, &preflight);
    let mut pump = spawn_pump(&live, &producer);

    // Input events from the editor thread.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
    std::thread::spawn(move || {
        loop {
            match editor.readline("❯ ") {
                Ok(line) => {
                    let _ = editor.add_history_entry(&line);
                    if line_tx.send(InputEvent::Line(line)).is_err() {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    if line_tx.send(InputEvent::CtrlC).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = line_tx.send(InputEvent::Eof);
                    break;
                }
            }
        }
    });

    while let Some(event) = line_rx.recv().await {
        match event {
            InputEvent::Eof => break,
            InputEvent::CtrlC => {
                if live.is_busy() {
                    live.cancel_turn().await;
                    producer.cancelled();
                } else {
                    producer.line("(Ctrl-C: cancels a running turn; /exit quits)");
                }
            }
            InputEvent::Line(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                match parse_meta(&line) {
                    Some(Meta::Exit) => break,
                    Some(Meta::Help) => help(&producer),
                    Some(Meta::Session) => {
                        producer.text(&format_session_detail(&live.session.snapshot()));
                    }
                    Some(Meta::Sessions) => {
                        match list_sessions(20) {
                            Ok(entries) => {
                                for (i, e) in entries.iter().enumerate() {
                                    producer.line(&format_session_list_line(i + 1, e));
                                }
                            }
                            Err(e) => producer.error_section(&e),
                        };
                    }
                    Some(Meta::Hosts) => {
                        for h in live.harness().host_status() {
                            let state = match (&h.error, h.connected, h.in_process) {
                                (Some(e), ..) => format!("DOWN ({e})"),
                                (None, _, true) => "ok (in-process)".into(),
                                (None, true, _) => "ok".into(),
                                (None, false, _) => "idle".into(),
                            };
                            producer.line(&format!("{}: {state}", h.name));
                        }
                    }
                    Some(Meta::Title(t)) => {
                        let done = live.session.with_mut(|s| s.set_title(t.clone()));
                        if let Err(e) = done {
                            producer.error_section(&e);
                        }
                    }
                    Some(Meta::Archive(archived)) => {
                        let done = live.session.with_mut(|s| {
                            s.archived = archived;
                            s.touch();
                            s.save()
                        });
                        match done {
                            Ok(()) => producer.line(if archived {
                                "archived (hidden from /sessions)"
                            } else {
                                "unarchived"
                            }),
                            Err(e) => producer.error_section(&e),
                        }
                    }
                    Some(Meta::Compact) => {
                        producer.line("compacting…");
                        let _ = live.tx.send(Cmd::Compact);
                        submitted += 1;
                    }
                    Some(Meta::New) => {
                        pump.abort();
                        live = boot_fresh(&sup, Session::new(default_model.clone())).await;
                        submitted = *live.turns.borrow();
                        producer = make_producer(&live, terminal.clone(), colors, wrap);
                        banner(&producer, &sup, &live, &preflight);
                        pump = spawn_pump(&live, &producer);
                    }
                    Some(Meta::Resume(id)) => match sup.ensure_live(&id, None).await {
                        Ok(l) => {
                            pump.abort();
                            live = l;
                            submitted = *live.turns.borrow();
                            producer = make_producer(&live, terminal.clone(), colors, wrap);
                            banner(&producer, &sup, &live, &preflight);
                            producer.replay_history(&live.session.snapshot().entries);
                            pump = spawn_pump(&live, &producer);
                        }
                        Err(e) => producer.error_section(&e),
                    },
                    None => {
                        submit(&sup, &live, &producer, &line);
                        submitted += 1;
                    }
                }
            }
        }
    }
    // Drain: let in-flight and queued turns finish before quitting, so an
    // exit right after a submit does not orphan the answer.
    let mut turns = live.turns.clone();
    if *turns.borrow() < submitted {
        eprintln!("(waiting for queued turns…)");
        while *turns.borrow() < submitted {
            if turns.changed().await.is_err() {
                break;
            }
        }
    }
    pump.abort();
    println!("session={}", live.session.id());
}

enum InputEvent {
    Line(String),
    CtrlC,
    Eof,
}

async fn boot_fresh(sup: &Arc<Server>, fresh: Session) -> Arc<Live> {
    let id = fresh.id.clone();
    match sup.ensure_live(&id, Some(fresh)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("myco: {e}");
            std::process::exit(1);
        }
    }
}

fn make_producer(
    live: &Arc<Live>,
    terminal: Arc<dyn TuiSink>,
    colors: bool,
    wrap: Option<usize>,
) -> Arc<TuiProducer> {
    let mirror = Arc::new(ConsoleTuiSink::new(ConsoleLog::new(
        live.session.clone(),
        true,
    )));
    Arc::new(TuiProducer::new(terminal, mirror, colors, wrap))
}

fn banner(
    producer: &Arc<TuiProducer>,
    sup: &Arc<Server>,
    live: &Arc<Live>,
    preflight: &StartupPreflight,
) {
    let snapshot = live.session.snapshot();
    producer.startup_banner(&snapshot.model, &snapshot.id);
    if preflight.has_problems() {
        producer.warning_section(&preflight.warning_body());
    }
    let _ = sup;
}

/// Print the USER header (context fill, running tools) and queue the turn.
fn submit(sup: &Arc<Server>, live: &Arc<Live>, producer: &Arc<TuiProducer>, line: &str) {
    let snapshot = live.session.snapshot();
    let max = sup
        .config()
        .models
        .get(&snapshot.model)
        .map(|m| m.spec.context_window_tokens)
        .unwrap_or(0);
    let used = match snapshot.last_usage {
        Some(u) => Some(u.context_tokens()),
        None if snapshot.entries.is_empty() => Some(0),
        None => None,
    };
    let running = live.harness().running_tool_summaries(uuid::Uuid::nil());
    producer.user_header(used, max, snapshot.last_usage, &running);
    producer.submitted_input(line);
    let _ = live.tx.send(Cmd::User {
        author: crate::server::local_author(),
        text: line.to_string(),
    });
}

/// Forward the session's event feed into the terminal renderer.
fn spawn_pump(live: &Arc<Live>, producer: &Arc<TuiProducer>) -> tokio::task::JoinHandle<()> {
    use myco_agent::EventSink;
    let mut rx = live.subscribe();
    let producer = producer.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(SessionEvent::Agent(e)) => producer.emit(e),
                Ok(SessionEvent::TurnFailed { message }) => producer.error_section(&message),
                Ok(SessionEvent::Compacted {
                    predecessor,
                    successor,
                }) => {
                    producer.line(&format!("COMPACTED {predecessor} → {successor}"));
                }
                Ok(SessionEvent::TurnStarted) | Ok(SessionEvent::TurnFinished) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

async fn run_print(live: Arc<Live>, prompt: String) {
    let mut turns = live.turns.clone();
    let start = *turns.borrow();
    if live
        .tx
        .send(Cmd::User {
            author: crate::server::local_author(),
            text: prompt,
        })
        .is_err()
    {
        eprintln!("myco: agent task gone");
        std::process::exit(1);
    }
    loop {
        if turns.changed().await.is_err() {
            eprintln!("myco: agent task gone");
            std::process::exit(1);
        }
        if *turns.borrow() > start {
            break;
        }
    }
    eprintln!("session={}", live.session.id());
    if let Some(err) = live.error() {
        eprintln!("myco: {err}");
        std::process::exit(1);
    }
    match last_answer(&live.session.snapshot().entries) {
        Some(text) => println!("{text}"),
        None => eprintln!("(no answer text)"),
    }
}

enum Meta {
    Help,
    Exit,
    New,
    Session,
    Sessions,
    Hosts,
    Resume(String),
    Title(Option<String>),
    Compact,
    /// `true` = archive, `false` = unarchive.
    Archive(bool),
}

fn parse_meta(input: &str) -> Option<Meta> {
    let (head, rest) = match input.split_once(char::is_whitespace) {
        Some((h, r)) => (h, Some(r.trim())),
        None => (input, None),
    };
    let cmd = head.strip_prefix('/')?;
    match (cmd, rest) {
        ("help", _) => Some(Meta::Help),
        ("exit" | "quit", _) => Some(Meta::Exit),
        ("new", _) => Some(Meta::New),
        ("session", _) => Some(Meta::Session),
        ("sessions", _) => Some(Meta::Sessions),
        ("hosts", _) => Some(Meta::Hosts),
        ("compact", _) => Some(Meta::Compact),
        ("archive", _) => Some(Meta::Archive(true)),
        ("unarchive", _) => Some(Meta::Archive(false)),
        ("resume", Some(id)) if !id.is_empty() => Some(Meta::Resume(id.to_string())),
        ("resume", _) => Some(Meta::Help),
        ("title", t) => Some(Meta::Title(
            t.map(|s| s.to_string()).filter(|s| !s.is_empty()),
        )),
        _ => Some(Meta::Help),
    }
}

fn help(producer: &Arc<TuiProducer>) {
    producer.line(
        "/new /resume <id> /sessions /session /title [text] /archive /unarchive \
         /compact /hosts /exit — input queues while a turn runs; Ctrl-C cancels \
         the running turn",
    );
}
