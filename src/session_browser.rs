//! `--mode session-browser`: a standalone session picker, plus the tmux
//! popup handshake bare `/resume` uses to launch it.
//!
//! The browser never runs inside the REPL's terminal. Inside tmux (>= 3.2,
//! `display-popup`) the REPL spawns `myco --mode session-browser --out FILE`
//! in a popup — its own pty, so rustyline state is untouched — and blocks
//! until the popup closes, then reads the chosen session id from FILE (the
//! same file-based handshake `fzf --tmux` uses; an empty or missing file
//! means the user cancelled). Outside tmux the same paged picker runs inline
//! on stdin/stdout, so tmux is an enhancement, not a dependency.
//!
//! Inside the browser, `fzf` (when installed) gives fuzzy search over
//! titles with a live transcript preview from the `{id}.console` mirror;
//! otherwise a plain paged prompt is used.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Stdio;

use crate::external_command;
use crate::session::{
    RECENT_SESSION_LIMIT, SessionListEntry, format_session_list_line, list_sessions,
    search_sessions, session_label, uuid_simple_hex,
};

/// Result cap for content search (`--search`, the picker's `s <text>`).
pub const SESSION_SEARCH_LIMIT: usize = 50;

/// Content-search hook for the paged picker: query → ranked entries.
pub type SessionSearchFn<'a> = &'a dyn Fn(&str) -> Result<Vec<SessionListEntry>, String>;

// ---------------------------------------------------------------------------
// Browser mode (`myco --mode session-browser`)
// ---------------------------------------------------------------------------

/// Entry point for `--mode session-browser`. Picks a visible session and
/// reports the choice: written to `out` when given (the popup handshake),
/// printed to stdout otherwise. Cancelling reports nothing and exits 0.
/// With `search`, the list is ranked by content match instead of recency.
pub fn run(out: Option<&Path>, search: Option<&str>) -> Result<(), String> {
    let all = list_sessions(0)?;
    if all.is_empty() {
        return Err("no sessions found under ~/.myco/session".into());
    }
    let entries = match search {
        Some(query) => {
            let report = search_sessions(&all, query, SESSION_SEARCH_LIMIT, false)?;
            if report.entries.is_empty() {
                return Err(format!("no sessions matched {query:?}"));
            }
            report.entries
        }
        None => all.clone(),
    };
    let search_fn =
        |query: &str| search_sessions(&all, query, SESSION_SEARCH_LIMIT, false).map(|r| r.entries);
    let choice = if external_command::FZF.is_installed() {
        pick_with_fzf(&entries)?
    } else {
        pick_paged(&entries, Some(&search_fn))?
    };
    match (choice, out) {
        (Some(id), Some(path)) => std::fs::write(path, id).map_err(|e| e.to_string()),
        (Some(id), None) => {
            println!("{id}");
            Ok(())
        }
        (None, _) => Ok(()),
    }
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
// Paged fallback picker
// ---------------------------------------------------------------------------

/// Plain paged picker on stdin/stdout: the browser's fallback when fzf is not
/// installed, and the REPL's inline picker outside tmux. Reads plain stdin
/// (not readline), so choices never enter readline history.
///
/// `search` powers the `s <text>` command: it swaps the working list for the
/// ranked results (bare `s` restores the original list). Returns the chosen
/// session id — or the raw id/prefix the user typed — and `None` on quit/EOF.
pub fn pick_paged(
    entries: &[SessionListEntry],
    search: Option<SessionSearchFn>,
) -> Result<Option<String>, String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    pick_paged_io(
        entries,
        RECENT_SESSION_LIMIT,
        search,
        &mut stdin.lock(),
        &mut stdout.lock(),
    )
}

fn pick_paged_io(
    entries: &[SessionListEntry],
    page_size: usize,
    search: Option<SessionSearchFn>,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<Option<String>, String> {
    if entries.is_empty() {
        return Ok(None);
    }
    let err = |e: std::io::Error| e.to_string();
    let mut current: Vec<SessionListEntry> = entries.to_vec();
    let mut shown = 0;
    print_next_page(&current, page_size, &mut shown, output)?;
    loop {
        let remaining = current.len() - shown;
        let more = if remaining > 0 {
            format!(", m = {remaining} more")
        } else {
            String::new()
        };
        let search_hint = if search.is_some() {
            ", s <text> = search"
        } else {
            ""
        };
        write!(
            output,
            "Resume which? [number/id/prefix{search_hint}{more}, q = quit] (empty = first listed): "
        )
        .map_err(err)?;
        output.flush().map_err(err)?;

        let mut line = String::new();
        if input.read_line(&mut line).map_err(err)? == 0 {
            return Ok(None); // EOF: treat like quit, never spin.
        }
        let choice = line.trim();
        match choice {
            "" => return Ok(Some(current[0].id.clone())),
            "q" | "quit" => return Ok(None),
            "m" | "more" => {
                if remaining == 0 {
                    writeln!(output, "(no more sessions)").map_err(err)?;
                } else {
                    print_next_page(&current, page_size, &mut shown, output)?;
                }
            }
            "s" => {
                current = entries.to_vec();
                shown = 0;
                writeln!(output, "(recent sessions)").map_err(err)?;
                print_next_page(&current, page_size, &mut shown, output)?;
            }
            _ if choice.starts_with("s ") => match search {
                None => writeln!(output, "(search not available here)").map_err(err)?,
                Some(f) => match f(choice.strip_prefix("s ").unwrap_or_default().trim()) {
                    Ok(results) if results.is_empty() => {
                        writeln!(output, "(no matches)").map_err(err)?;
                    }
                    Ok(results) => {
                        current = results;
                        shown = 0;
                        writeln!(output, "matches: {}  (s = back to recent)", current.len())
                            .map_err(err)?;
                        print_next_page(&current, page_size, &mut shown, output)?;
                    }
                    Err(e) => writeln!(output, "search failed: {e}").map_err(err)?,
                },
            },
            _ => {
                if let Ok(n) = choice.parse::<usize>() {
                    // Numbers address listed rows only — what's on screen.
                    match n.checked_sub(1).and_then(|i| current[..shown].get(i)) {
                        Some(entry) => return Ok(Some(entry.id.clone())),
                        None => writeln!(output, "no listed session numbered {n}").map_err(err)?,
                    }
                } else {
                    return Ok(Some(choice.to_string()));
                }
            }
        }
    }
}

fn print_next_page(
    entries: &[SessionListEntry],
    page_size: usize,
    shown: &mut usize,
    output: &mut dyn Write,
) -> Result<(), String> {
    let end = (*shown + page_size).min(entries.len());
    for (i, entry) in entries[*shown..end].iter().enumerate() {
        writeln!(
            output,
            "{}",
            format_session_list_line(*shown + i + 1, entry)
        )
        .map_err(|e| e.to_string())?;
    }
    *shown = end;
    Ok(())
}

// ---------------------------------------------------------------------------
// tmux popup handshake (REPL side)
// ---------------------------------------------------------------------------

/// Inside a tmux client whose tmux supports `display-popup` (>= 3.2)?
pub fn tmux_popup_available() -> bool {
    if std::env::var_os("TMUX").is_none_or(|v| v.is_empty()) {
        return false;
    }
    let Ok(out) = external_command::TMUX.command().arg("-V").output() else {
        return false;
    };
    out.status.success() && tmux_supports_popup(&String::from_utf8_lossy(&out.stdout))
}

/// First `major.minor` in `tmux -V` output vs 3.2. Handles suffixed ("3.2a")
/// and prefixed ("next-3.6") forms; unparseable output is unsupported.
fn tmux_supports_popup(version_line: &str) -> bool {
    let bytes = version_line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let major: u32 = version_line[start..i].parse().unwrap_or(0);
        if i < bytes.len() && bytes[i] == b'.' {
            let minor_start = i + 1;
            let mut j = minor_start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > minor_start {
                let minor: u32 = version_line[minor_start..j].parse().unwrap_or(0);
                return major > 3 || (major == 3 && minor >= 2);
            }
        }
        if major >= 4 {
            return true;
        }
    }
    false
}

/// Run the browser in a `tmux display-popup` and block until it closes.
/// `Ok(None)` = cancelled inside the popup; `Err` = tmux itself failed (the
/// caller falls back to the inline picker).
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
    use std::io::Cursor;
    use std::path::PathBuf;

    fn entry(id: &str, title: Option<&str>) -> SessionListEntry {
        SessionListEntry {
            id: id.to_string(),
            path: PathBuf::from(format!("/tmp/{id}.json")),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            model: "test-model".into(),
            message_count: 3,
            title: title.map(str::to_string),
            snippet: "first user message".into(),
            link_counts: LinkCounts::default(),
            kind: SessionKind::User,
            parent_session_id: None,
        }
    }

    fn pick(entries: &[SessionListEntry], page_size: usize, input: &str) -> Option<String> {
        pick_with_search(entries, page_size, input, None).0
    }

    fn pick_with_search(
        entries: &[SessionListEntry],
        page_size: usize,
        input: &str,
        search: Option<SessionSearchFn>,
    ) -> (Option<String>, String) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        let got = pick_paged_io(entries, page_size, search, &mut reader, &mut out).unwrap();
        (got, String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn popup_gate_requires_tmux_3_2() {
        assert!(!tmux_supports_popup("tmux 3.1c"));
        assert!(tmux_supports_popup("tmux 3.2"));
        assert!(tmux_supports_popup("tmux 3.2a"));
        assert!(tmux_supports_popup("tmux 3.10")); // numeric, not lexicographic
        assert!(tmux_supports_popup("tmux next-3.6"));
        assert!(!tmux_supports_popup("tmux"));
        assert!(!tmux_supports_popup(""));
    }

    #[test]
    fn sh_quote_survives_embedded_single_quotes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("it's"), r#"'it'\''s'"#);
    }

    #[test]
    fn fzf_line_is_three_tab_fields_with_sanitized_display() {
        let e = entry("deadbeef00112233", Some("tabs\tand\nnewlines"));
        let line = fzf_line(&e);
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 3, "line: {line:?}");
        assert_eq!(fields[0], "deadbeef00112233");
        assert!(fields[1].ends_with(".console"));
        assert!(fields[2].contains("tabs and newlines"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn paged_empty_input_picks_most_recent() {
        let entries = vec![entry("aaa", None), entry("bbb", None)];
        assert_eq!(pick(&entries, 10, "\n"), Some("aaa".into()));
    }

    #[test]
    fn paged_quit_and_eof_cancel() {
        let entries = vec![entry("aaa", None)];
        assert_eq!(pick(&entries, 10, "q\n"), None);
        assert_eq!(pick(&entries, 10, ""), None);
        assert_eq!(pick(&[], 10, "\n"), None);
    }

    #[test]
    fn paged_more_reveals_next_page_and_numbers_stay_global() {
        let entries = vec![entry("aaa", None), entry("bbb", None), entry("ccc", None)];
        assert_eq!(pick(&entries, 2, "m\n3\n"), Some("ccc".into()));
    }

    #[test]
    fn paged_number_beyond_listed_rows_reprompts() {
        let entries = vec![entry("aaa", None), entry("bbb", None), entry("ccc", None)];
        // Page size 2: "3" is not on screen yet → re-prompt, then quit.
        assert_eq!(pick(&entries, 2, "3\nq\n"), None);
    }

    #[test]
    fn paged_non_number_is_returned_as_prefix() {
        let entries = vec![entry("aaa", None)];
        assert_eq!(pick(&entries, 10, "deadbeef\n"), Some("deadbeef".into()));
    }

    type SearchResult = Result<Vec<SessionListEntry>, String>;

    #[test]
    fn paged_search_swaps_ranking_and_empty_picks_top_match() {
        let entries = vec![entry("aaa", None), entry("bbb", None)];
        let ranked = vec![entry("ccc", None)];
        let search = |_q: &str| -> SearchResult { Ok(ranked.clone()) };
        let (got, _) = pick_with_search(&entries, 10, "s foo\n\n", Some(&search));
        assert_eq!(got, Some("ccc".into()));
    }

    #[test]
    fn paged_search_no_matches_keeps_current_list() {
        let entries = vec![entry("aaa", None)];
        let search = |_q: &str| -> SearchResult { Ok(Vec::new()) };
        let (got, out) = pick_with_search(&entries, 10, "s foo\n\n", Some(&search));
        assert_eq!(got, Some("aaa".into()));
        assert!(out.contains("(no matches)"), "{out}");
    }

    #[test]
    fn paged_bare_s_restores_recency_list() {
        let entries = vec![entry("aaa", None), entry("bbb", None)];
        let ranked = vec![entry("ccc", None)];
        let search = |_q: &str| -> SearchResult { Ok(ranked.clone()) };
        let (got, _) = pick_with_search(&entries, 10, "s foo\ns\n\n", Some(&search));
        assert_eq!(got, Some("aaa".into()));
    }
}
