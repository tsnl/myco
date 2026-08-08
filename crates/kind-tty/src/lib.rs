//! The `tty` kind: a terminal as an instance, in two materials.
//!
//! **pty mode** (default) is the interactive terminal: a vt100 screen
//! model plus bounded raw scrollback, fed by a side-feed reading the pty
//! master. **piped mode** (`{"mode": "piped"}`) is the one-shot workhorse:
//! plain pipes, no line discipline, and — what a pty cannot give — the
//! child's **exit status**, reported in `text` and the `exited` event.
//! Both modes share one verb surface and one byte-stream state — held in
//! the framework's [`Shared`], so the byte pumps mutate it and publish in
//! one move while the read verbs observe without publishing. The
//! exit-status authority is a waiter task that reaps the child, so
//! `running: false` and the exit code land together, atomically.
//!
//! Two shapes of read serve two purposes — `screen` (styled cells, for
//! terminal renderers) and `tail` (raw scrollback from a cursor) — plus
//! `text`, the plain-text projection every kind owes its consumers.
//! `input`, `resize`, and `signal` are driver-gated: one principal drives,
//! everyone watches. `signal` delivers to the child's process group, the
//! same reach a real terminal's Ctrl-C has.

use std::os::unix::process::ExitStatusExt as _;

use myco_instance::{Instance, Kind, KindSpec, Principal, Shared, VerbError, VerbSpec};
use myco_runtime::Signals;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

mod pty;
mod screen;

static TTY_SPEC: KindSpec = KindSpec {
    kind: "tty",
    version: 1,
    doc: "a terminal: a pty running a command, watched by anyone, typed into by its driver",
    verbs: &[
        VerbSpec::driven(
            "input",
            "write {data} to the terminal as keystrokes; in piped mode {eof: true} closes stdin",
        ),
        VerbSpec::driven(
            "resize",
            "set the terminal to {cols}×{rows} (pty mode only)",
        ),
        VerbSpec::driven(
            "signal",
            "send {signal} (INT, TERM, KILL, HUP, QUIT, USR1, USR2, STOP, CONT) to the \
             child's process group",
        ),
        VerbSpec::read("screen", "the rendered screen: styled runs, cursor, size"),
        VerbSpec::read(
            "text",
            "the screen as plain text, whether the child runs, and its exit {code}/{signal} \
             once it doesn't",
        ),
        VerbSpec::cursored_read(
            "tail",
            "output from byte offset {from}, at most {max_bytes}; returns {next}. Offsets are \
             raw-byte positions: compare the returned {from} to what you sent to detect \
             scrollback trimming, and never derive cursors from the decoded string's length \
             (decoding is lossy at chunk boundaries)",
        ),
    ],
    primary_render: "screen",
    recommended_context: "text",
};

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// Raw scrollback kept for `tail`. Old bytes fall off the front; offsets are
/// absolute since birth, so a cursor survives the trimming.
const SCROLLBACK_CAP: usize = 512 * 1024;

/// What the reader task feeds and the read verbs consume.
struct Term {
    parser: vt100::Parser,
    scroll: Vec<u8>,
    scroll_start: u64,
    running: bool,
    /// Set by the waiter, together with `running: false` — the two are one
    /// fact and land under one lock.
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
}

pub struct TtyKind;

impl Kind for TtyKind {
    fn spec(&self) -> &'static KindSpec {
        &TTY_SPEC
    }

    fn create(
        &self,
        _ctx: &myco_instance::CreateCtx,
        args: Value,
        signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string);
        let cols = dimension(&args, "cols", DEFAULT_COLS, 20, 500)?;
        let rows = dimension(&args, "rows", DEFAULT_ROWS, 5, 200)?;
        let cwd = args.get("cwd").and_then(Value::as_str).map(str::to_string);
        match args.get("mode").and_then(Value::as_str).unwrap_or("pty") {
            "pty" => {}
            "piped" => {
                let command = command.ok_or_else(|| bad("piped mode needs {command}"))?;
                return create_piped(command, cwd, cols, rows, signals);
            }
            other => {
                return Err(bad(&format!(
                    "mode must be \"pty\" or \"piped\", got {other:?}"
                )));
            }
        }

        let (reader, writer, slave) = pty::open(cols, rows).map_err(failed)?;
        let (sin, sout) = match (slave.try_clone(), slave.try_clone()) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return Err(failed(format!("pty slave dup: {e}"))),
        };
        let mut cmd = tokio::process::Command::new("bash");
        // An explicit command runs under `bash -c`; the default is a plain
        // interactive shell, not a shell running a shell.
        match &command {
            Some(c) => cmd.args(["-c", c]),
            None => cmd.arg("-i"),
        };
        cmd.stdin(std::process::Stdio::from(sin))
            .stdout(std::process::Stdio::from(sout))
            .stderr(std::process::Stdio::from(slave))
            .kill_on_drop(true)
            .env("TERM", "xterm-256color");
        // setsid puts the child in its own fresh process group with the
        // slave as controlling tty, so signals and kill(-pid) behave like a
        // real terminal's.
        unsafe {
            cmd.pre_exec(|| pty::make_controlling_tty());
        }
        if let Some(dir) = &cwd {
            cmd.current_dir(dir);
        }
        let shown = command.as_deref().unwrap_or("bash -i");
        let child = cmd
            .spawn()
            .map_err(|e| failed(format!("spawn {shown:?}: {e}")))?;
        let pgid = child.id().map(|pid| pid as i32);

        let term = Shared::new(Term::new(rows, cols), signals);
        let pumps = vec![tokio::spawn(pump(reader, term.clone()))];
        tokio::spawn(waiter(child, pumps, term.clone()));

        Ok(Box::new(Tty {
            input: InputSink::Pty(writer),
            pgid,
            term,
        }))
    }
}

/// The one-shot material: plain pipes, a process group of its own, and an
/// exit status. stdout and stderr pump into the same byte stream the pty
/// would fill (interleaved at chunk granularity), so every read verb
/// answers identically in both modes.
fn create_piped(
    command: String,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    signals: Signals,
) -> Result<Box<dyn Instance>, VerbError> {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.args(["-c", &command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0);
    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| failed(format!("spawn {command:?}: {e}")))?;
    let pgid = child.id().map(|pid| pid as i32);
    let stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failed("no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| failed("no stderr pipe"))?;

    let term = Shared::new(Term::new(rows, cols), signals);
    let pumps = vec![
        tokio::spawn(pipe_pump(stdout, term.clone())),
        tokio::spawn(pipe_pump(stderr, term.clone())),
    ];
    tokio::spawn(waiter(child, pumps, term.clone()));

    Ok(Box::new(Tty {
        input: InputSink::Piped(stdin),
        pgid,
        term,
    }))
}

impl Term {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            scroll: Vec::new(),
            scroll_start: 0,
            running: true,
            exit_code: None,
            exit_signal: None,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.scroll.extend_from_slice(bytes);
        if self.scroll.len() > SCROLLBACK_CAP {
            let excess = self.scroll.len() - SCROLLBACK_CAP;
            self.scroll_start += excess as u64;
            self.scroll.drain(..excess);
        }
    }
}

/// The exit-status authority: reap the child, wait for every pump to
/// drain (a pipe can still hold bytes after the exit), then land
/// `running: false` and the status under one lock and say so. The single
/// source of the `exited` event in both modes — and the guarantee that
/// when `running` flips, the scrollback is complete.
async fn waiter(
    mut child: tokio::process::Child,
    pumps: Vec<tokio::task::JoinHandle<()>>,
    term: Shared<Term>,
) {
    let status = child.wait().await;
    for pump in pumps {
        let _ = pump.await;
    }
    let (code, signal) = match &status {
        Ok(s) => (s.code(), s.signal()),
        Err(_) => (None, None),
    };
    term.with(|t| {
        t.running = false;
        t.exit_code = code;
        t.exit_signal = signal;
    });
    term.signals()
        .emit("exited", json!({ "code": code, "signal": signal }));
}

/// A pipe's half of the piped-mode stream.
async fn pipe_pump(
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    term: Shared<Term>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => term.with(|t| t.feed(&buf[..n])),
        }
    }
}

/// The pty side-feed: bytes → screen model + scrollback, one bump per
/// chunk. EOF (or EIO, the pty spelling of it) ends the pump; the *waiter*
/// owns `running` and the `exited` event, so status and bytes cannot race.
async fn pump(mut reader: pty::PtyReader, term: Shared<Term>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => term.with(|t| t.feed(&buf[..n])),
        }
    }
}

/// Where keystrokes go: the pty master, or the piped child's stdin
/// (`None` once closed by `input {eof: true}`).
enum InputSink {
    Pty(pty::PtyWriter),
    Piped(Option<tokio::process::ChildStdin>),
}

struct Tty {
    input: InputSink,
    /// The child's process group (setsid'd in pty mode, process_group(0)
    /// in piped mode) — `signal`'s target, and Drop's.
    pgid: Option<i32>,
    term: Shared<Term>,
}

impl Drop for Tty {
    /// The waiter owns the child handle, so removal must kill the process
    /// group explicitly — including everything the command spawned.
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

#[async_trait::async_trait]
impl Instance for Tty {
    async fn verb(
        &mut self,
        _caller: &Principal,
        verb: &str,
        args: Value,
        _signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "input" => {
                if args.get("eof") == Some(&json!(true)) {
                    match &mut self.input {
                        InputSink::Piped(stdin) => {
                            // Dropping the handle is the EOF.
                            stdin.take();
                            return Ok(Value::Null);
                        }
                        InputSink::Pty(_) => {
                            return Err(bad(
                                "eof is a piped-mode concept; a pty closes with the child",
                            ));
                        }
                    }
                }
                let data = args
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad("input needs {data}"))?;
                match &mut self.input {
                    InputSink::Pty(writer) => {
                        writer.write_all(data.as_bytes()).await.map_err(failed)?;
                    }
                    InputSink::Piped(Some(stdin)) => {
                        stdin.write_all(data.as_bytes()).await.map_err(failed)?;
                        stdin.flush().await.map_err(failed)?;
                    }
                    InputSink::Piped(None) => {
                        return Err(failed("stdin is closed (eof was sent)"));
                    }
                }
                Ok(Value::Null)
            }
            "resize" => {
                let InputSink::Pty(writer) = &mut self.input else {
                    return Err(bad("piped terminals have no size"));
                };
                let cols = dimension(&args, "cols", 0, 20, 500)?;
                let rows = dimension(&args, "rows", 0, 5, 200)?;
                writer.resize(cols, rows).map_err(failed)?;
                self.term.with(|t| t.parser.set_size(rows, cols));
                Ok(Value::Null)
            }
            "signal" => {
                let name = args
                    .get("signal")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad("signal needs {signal}, e.g. \"INT\""))?;
                let sig = signal_number(name)?;
                let pgid = self.pgid.ok_or_else(|| failed("no process group"))?;
                let delivered = unsafe { libc::kill(-pgid, sig) } == 0;
                Ok(json!({ "delivered": delivered }))
            }
            "screen" => Ok(self.term.read(|t| {
                serde_json::to_value(screen::render(t.parser.screen())).expect("serializes")
            })),
            "text" => Ok(self.term.read(|t| {
                json!({
                    "text": t.parser.screen().contents(),
                    "running": t.running,
                    "exit_code": t.exit_code,
                    "exit_signal": t.exit_signal,
                })
            })),
            "tail" => {
                // The budget doctrine made real: the caller bounds its own
                // read; `next` reports how far this reply actually got.
                let from = args.get("from").and_then(Value::as_u64).unwrap_or(0);
                let max = args
                    .get("max_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX);
                Ok(self.term.read(|t| {
                    let end = t.scroll_start + t.scroll.len() as u64;
                    let from = from.clamp(t.scroll_start, end);
                    let upto = end.min(from.saturating_add(max));
                    let slice = &t.scroll
                        [(from - t.scroll_start) as usize..(upto - t.scroll_start) as usize];
                    // Best-effort UTF-8: a cursor or trim landing mid-codepoint
                    // decodes as U+FFFD at the seam. Cursor math stays in bytes.
                    let data = String::from_utf8_lossy(slice).into_owned();
                    json!({ "from": from, "data": data, "next": upto })
                }))
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

/// Signal names → numbers, the ones a terminal's keyboard could send plus
/// the polite/impolite kill pair. Accepts with or without the SIG prefix.
fn signal_number(name: &str) -> Result<i32, VerbError> {
    let bare = name.trim().to_ascii_uppercase();
    let bare = bare.strip_prefix("SIG").unwrap_or(&bare);
    Ok(match bare {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        other => {
            return Err(bad(&format!(
                "unknown signal {other:?}; one of HUP, INT, QUIT, KILL, TERM, USR1, USR2, \
                 STOP, CONT"
            )));
        }
    })
}

fn dimension(args: &Value, key: &str, default: u16, min: u16, max: u16) -> Result<u16, VerbError> {
    let raw = match args.get(key) {
        None if default > 0 => return Ok(default),
        None => return Err(bad(&format!("missing {{{key}}}"))),
        Some(v) => v
            .as_u64()
            .ok_or_else(|| bad(&format!("{key} must be a number")))?,
    };
    u16::try_from(raw)
        .ok()
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| bad(&format!("{key} must be {min}..={max}")))
}

fn bad(why: &str) -> VerbError {
    VerbError::BadArgs { why: why.into() }
}

fn failed(why: impl std::fmt::Display) -> VerbError {
    VerbError::Failed {
        why: why.to_string(),
    }
}
