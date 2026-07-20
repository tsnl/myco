//! End-to-end check for the per-session console mirror ([`myco::ConsoleLog`]):
//! it strips ANSI, writes to `{id}.console`, and follows a session swap
//! (`/new`, `/compact`, `/resume`) to the new file.
//!
//! Sole test in its own binary so the process-global `MYCO_HOME` override does
//! not race other tests.

use myco::tui::{ConsoleTuiSink, Style, TuiEvent, TuiSink};
use myco::{ActiveSession, ConsoleLog, Session};

#[test]
fn mirror_strips_ansi_and_follows_session_swap() {
    let dir = std::env::temp_dir().join(format!("myco-console-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // SAFETY: single-threaded test start, sole test in this binary.
    unsafe { std::env::set_var("MYCO_HOME", &dir) };

    let first = Session::new("m");
    let first_console = first.console_path();
    let active = ActiveSession::new(first);
    let log = ConsoleLog::new(active.clone(), /*enabled*/ true);

    // Styled bytes in → plain bytes on disk, across multiple appends.
    log.append("\x1b[0;1;36mUSER\x1b[0m\n");
    log.append("hello \x1b[1mworld\x1b[0m\n");
    assert_eq!(
        std::fs::read_to_string(&first_console).unwrap(),
        "USER\nhello world\n"
    );

    // Swap the active session (as `/new` does): the mirror redirects to the
    // new session's file with no extra wiring.
    let second = Session::new("m");
    let second_console = second.console_path();
    assert_ne!(first_console, second_console);
    active.replace(second);
    log.append("fresh session\n");

    assert_eq!(
        std::fs::read_to_string(&second_console).unwrap(),
        "fresh session\n"
    );
    // The first session's file is untouched by post-swap writes.
    assert_eq!(
        std::fs::read_to_string(&first_console).unwrap(),
        "USER\nhello world\n"
    );

    // A disabled mirror writes nothing even when the session exists.
    let third = Session::new("m");
    let third_console = third.console_path();
    let disabled = ConsoleLog::new(ActiveSession::new(third), /*enabled*/ false);
    disabled.append("should not appear\n");
    assert!(!third_console.exists());

    // TuiEvent path (design sketch): a ConsoleTuiSink subscribed to the TUI
    // stream lands plain text in the same {id}.console file — style events
    // are simply never encoded, no stripping involved.
    let fourth = Session::new("m");
    let fourth_console = fourth.console_path();
    let sink = ConsoleTuiSink::new(ConsoleLog::new(
        ActiveSession::new(fourth),
        /*enabled*/ true,
    ));
    sink.emit(&[
        TuiEvent::Style(Style::USER),
        TuiEvent::Text("USER 0/100\n".into()),
        TuiEvent::Style(Style::RESET),
        TuiEvent::Text("hi **there**\n".into()),
    ]);
    assert_eq!(
        std::fs::read_to_string(&fourth_console).unwrap(),
        "USER 0/100\nhi **there**\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
