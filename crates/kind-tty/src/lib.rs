//! The `tty` kind: a pty-backed terminal as an instance.
//!
//! State is a vt100 screen model plus a bounded raw scrollback, fed by a
//! side-feed task reading the pty master (the sanctioned pattern: commands
//! serialize through the cell, the byte stream flows outside it and
//! publishes through [`Signals`]). Two shapes of read serve two purposes —
//! `screen` (styled cells, for terminal renderers) and `tail` (raw
//! scrollback from a cursor, for log-following) — plus `text`, the
//! plain-text projection every kind owes its consumers. `input` and
//! `resize` are driver-gated: one principal types at a time, everyone
//! watches freely.

use std::sync::{Arc, Mutex};

use myco_instance::{Instance, Kind, KindSpec, Principal, VerbError, VerbSpec};
use myco_runtime::Signals;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;

mod pty;
mod screen;

static TTY_SPEC: KindSpec = KindSpec {
    kind: "tty",
    version: 1,
    doc: "a terminal: a pty running a command, watched by anyone, typed into by its driver",
    verbs: &[
        VerbSpec::driven("input", "write {data} to the terminal as keystrokes"),
        VerbSpec::driven("resize", "set the terminal to {cols}×{rows}"),
        VerbSpec::read("screen", "the rendered screen: styled runs, cursor, size"),
        VerbSpec::read(
            "text",
            "the screen as plain text, plus whether the child runs",
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
}

pub struct TtyKind;

impl Kind for TtyKind {
    fn spec(&self) -> &'static KindSpec {
        &TTY_SPEC
    }

    fn create(&self, args: Value, signals: Signals) -> Result<Box<dyn Instance>, VerbError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string);
        let cols = dimension(&args, "cols", DEFAULT_COLS, 20, 500)?;
        let rows = dimension(&args, "rows", DEFAULT_ROWS, 5, 200)?;
        let cwd = args.get("cwd").and_then(Value::as_str).map(str::to_string);

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

        let term = Arc::new(Mutex::new(Term {
            parser: vt100::Parser::new(rows, cols, 0),
            scroll: Vec::new(),
            scroll_start: 0,
            running: true,
        }));
        tokio::spawn(pump(reader, Arc::clone(&term), signals));

        Ok(Box::new(Tty {
            writer,
            _child: child,
            term,
        }))
    }
}

/// The side-feed: pty bytes → screen model + scrollback, one bump per
/// chunk. EOF (or EIO, the pty spelling of it) means the child is gone.
async fn pump(mut reader: pty::PtyReader, term: Arc<Mutex<Term>>, signals: Signals) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                {
                    let mut t = term.lock().unwrap_or_else(|e| e.into_inner());
                    t.parser.process(&buf[..n]);
                    t.scroll.extend_from_slice(&buf[..n]);
                    if t.scroll.len() > SCROLLBACK_CAP {
                        let excess = t.scroll.len() - SCROLLBACK_CAP;
                        t.scroll_start += excess as u64;
                        t.scroll.drain(..excess);
                    }
                }
                signals.bump();
            }
        }
    }
    term.lock().unwrap_or_else(|e| e.into_inner()).running = false;
    signals.emit("exited", Value::Null);
    signals.bump();
}

struct Tty {
    writer: pty::PtyWriter,
    /// Held only so `kill_on_drop` fires when the instance is removed.
    _child: tokio::process::Child,
    term: Arc<Mutex<Term>>,
}

#[async_trait::async_trait]
impl Instance for Tty {
    async fn verb(
        &mut self,
        _caller: &Principal,
        verb: &str,
        args: Value,
        signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "input" => {
                let data = args
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad("input needs {data}"))?;
                self.writer
                    .write_all(data.as_bytes())
                    .await
                    .map_err(failed)?;
                Ok(Value::Null)
            }
            "resize" => {
                let cols = dimension(&args, "cols", 0, 20, 500)?;
                let rows = dimension(&args, "rows", 0, 5, 200)?;
                self.writer.resize(cols, rows).map_err(failed)?;
                self.term
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .parser
                    .set_size(rows, cols);
                signals.bump();
                Ok(Value::Null)
            }
            "screen" => {
                let t = self.term.lock().unwrap_or_else(|e| e.into_inner());
                Ok(serde_json::to_value(screen::render(t.parser.screen())).expect("serializes"))
            }
            "text" => {
                let t = self.term.lock().unwrap_or_else(|e| e.into_inner());
                Ok(json!({
                    "text": t.parser.screen().contents(),
                    "running": t.running,
                }))
            }
            "tail" => {
                // The budget doctrine made real: the caller bounds its own
                // read; `next` reports how far this reply actually got.
                let from = args.get("from").and_then(Value::as_u64).unwrap_or(0);
                let max = args
                    .get("max_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX);
                let t = self.term.lock().unwrap_or_else(|e| e.into_inner());
                let end = t.scroll_start + t.scroll.len() as u64;
                let from = from.clamp(t.scroll_start, end);
                let upto = end.min(from.saturating_add(max));
                let slice =
                    &t.scroll[(from - t.scroll_start) as usize..(upto - t.scroll_start) as usize];
                // Best-effort UTF-8: a cursor or trim landing mid-codepoint
                // decodes as U+FFFD at the seam. Cursor math stays in bytes.
                let data = String::from_utf8_lossy(slice).into_owned();
                Ok(json!({ "from": from, "data": data, "next": upto }))
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
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
