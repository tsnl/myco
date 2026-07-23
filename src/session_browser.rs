//! `--mode session-browser`: an fzf session picker, plus the tmux popup
//! handshake bare `/resume` uses to launch it.
//!
//! `tmux` and `fzf` are expected on PATH (the startup preflight warns when
//! either is missing; `/resume <id|prefix>` never needs them). Inside tmux
//! the REPL runs the browser via `display-popup -E` — its own pty, so
//! rustyline state is untouched — and blocks on the `--out` file (the same
//! file handshake `fzf --tmux` uses; an empty or missing file means the user
//! cancelled). Outside tmux, or when the popup fails (e.g. tmux < 3.2), fzf
//! runs in the current terminal instead.
//!
//! fzf's own typing filters the display labels; `--search` ranks the list by
//! content match ([`search_sessions`]) before fzf ever starts.

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use crate::external_command;
use crate::session::{
    SessionListEntry, list_sessions, search_sessions, session_label, uuid_simple_hex,
};

/// Result cap for content search (`--search`).
pub const SESSION_SEARCH_LIMIT: usize = 50;

// ---------------------------------------------------------------------------
// Browser mode (`myco --mode session-browser`)
// ---------------------------------------------------------------------------

/// Entry point for `--mode session-browser`. Picks a visible session and
/// reports the choice: written to `out` when given (the popup handshake),
/// printed to stdout otherwise. Cancelling reports nothing and exits 0.
/// With `search`, the list is ranked by content match instead of recency.
pub fn run(out: Option<&Path>, search: Option<&str>) -> Result<(), String> {
    match (pick(search)?, out) {
        (Some(id), Some(path)) => std::fs::write(path, id).map_err(|e| e.to_string()),
        (Some(id), None) => {
            println!("{id}");
            Ok(())
        }
        (None, _) => Ok(()),
    }
}

/// List (or, with `search`, rank) visible sessions and pick one via fzf.
/// `Ok(None)` = cancelled.
pub fn pick(search: Option<&str>) -> Result<Option<String>, String> {
    let all = list_sessions(0)?;
    if all.is_empty() {
        return Err("no sessions found under ~/.myco/session".into());
    }
    let entries = match search {
        Some(query) => {
            let report = search_sessions(&all, query, SESSION_SEARCH_LIMIT)?;
            if report.entries.is_empty() {
                return Err(format!("no sessions matched {query:?}"));
            }
            report.entries
        }
        None => all,
    };
    pick_with_fzf(&entries)
}

/// Fuzzy pick via fzf. `Ok(None)` on cancel (Esc/Ctrl-C) or no match.
fn pick_with_fzf(entries: &[SessionListEntry]) -> Result<Option<String>, String> {
    let mut child = external_command::FZF
        .command()
        .args([
            "--delimiter",
            "\t",
            "--with-nth",
            "3..",
            "--layout",
            "reverse",
            "--prompt",
            "session> ",
            "--header",
            "enter = pick   esc = cancel",
            "--preview",
            "tail -n 100 {2} 2>/dev/null || echo '(no console transcript)'",
            "--preview-window",
            "right:55%",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn fzf: {e}"))?;

    {
        // Take (not borrow) stdin so dropping it sends EOF. A write error just
        // means fzf already exited (e.g. instant Esc) — the wait sorts it out.
        let mut stdin = child.stdin.take().ok_or("fzf stdin unavailable")?;
        for entry in entries {
            if writeln!(stdin, "{}", fzf_line(entry)).is_err() {
                break;
            }
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    match output.status.code() {
        // 1 = no match for the query, 130 = interrupted (Esc / Ctrl-C).
        Some(1) | Some(130) => return Ok(None),
        Some(0) => {}
        _ => return Err("fzf failed".into()),
    }
    let line = String::from_utf8_lossy(&output.stdout);
    Ok(line
        .trim()
        .split('\t')
        .next()
        .filter(|id| !id.is_empty())
        .map(str::to_string))
}

/// `id \t console-path \t display` — fzf shows and matches only the display
/// field (`--with-nth 3..`); the preview tails the console path.
fn fzf_line(entry: &SessionListEntry) -> String {
    format!(
        "{}\t{}\t{}",
        entry.id,
        entry.path.with_extension("console").display(),
        sanitize_field(&display_line(entry)),
    )
}

/// Label first (what humans scan and search), then metadata, id prefix last
/// so exact-prefix queries still work.
fn display_line(entry: &SessionListEntry) -> String {
    let time = entry
        .updated_at
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M");
    let links = if entry.link_counts.is_empty() {
        String::new()
    } else {
        format!(
            "  pr:{} wt:{}",
            entry.link_counts.prs, entry.link_counts.worktrees
        )
    };
    let short_id: String = entry.id.chars().take(8).collect();
    format!(
        "{}  |  {}  {}  msgs={}{}  {}",
        session_label(entry),
        time,
        entry.model,
        entry.message_count,
        links,
        short_id,
    )
}

/// One line, tab-free, so embedded titles cannot break the field format.
fn sanitize_field(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() || c == '\t' { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------
// tmux popup handshake (REPL side)
// ---------------------------------------------------------------------------

/// Bare `/resume` uses the popup only when the CLI runs inside tmux.
pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

/// Run the browser in a `tmux display-popup` and block until it closes.
/// `Ok(None)` = cancelled inside the popup; `Err` = tmux itself failed (the
/// caller falls back to fzf in the current terminal).
pub fn pick_via_tmux_popup() -> Result<Option<String>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate myco executable: {e}"))?;
    let result_path = std::env::temp_dir().join(format!(
        "myco-session-pick-{}",
        uuid_simple_hex(uuid::Uuid::new_v4())
    ));
    let popup_cmd = format!(
        "{} --mode session-browser --out {}",
        sh_quote(&exe.to_string_lossy()),
        sh_quote(&result_path.to_string_lossy()),
    );
    // -E closes the popup when the command exits; the tmux client blocks
    // until then, so waiting on it is the synchronization.
    let status = external_command::TMUX
        .command()
        .args(["display-popup", "-E", "-w", "90%", "-h", "80%"])
        .arg(&popup_cmd)
        .status()
        .map_err(|e| format!("failed to run tmux: {e}"))?;
    let choice = std::fs::read_to_string(&result_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let _ = std::fs::remove_file(&result_path);
    if choice.is_none() && !status.success() {
        return Err("tmux display-popup failed".into());
    }
    Ok(choice)
}

/// POSIX single-quote escaping for embedding paths in the popup command.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{LinkCounts, SessionKind};
    use std::path::PathBuf;

    #[test]
    fn sh_quote_survives_embedded_single_quotes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("it's"), r#"'it'\''s'"#);
    }

    #[test]
    fn fzf_line_is_three_tab_fields_with_sanitized_display() {
        let entry = SessionListEntry {
            id: "deadbeef00112233".into(),
            path: PathBuf::from("/tmp/deadbeef00112233.json"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            model: "test-model".into(),
            message_count: 3,
            title: Some("tabs\tand\nnewlines".into()),
            snippet: "first user message".into(),
            link_counts: LinkCounts::default(),
            kind: SessionKind::User,
            parent_session_id: None,
        };
        let line = fzf_line(&entry);
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 3, "line: {line:?}");
        assert_eq!(fields[0], "deadbeef00112233");
        assert!(fields[1].ends_with(".console"));
        assert!(fields[2].contains("tabs and newlines"));
        assert!(!line.contains('\n'));
    }
}
