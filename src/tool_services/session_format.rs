//! Plain-text session rows and details for the `session_meta` and
//! `list_recent` tool results.

use crate::session::{Session, SessionLink, SessionListEntry, session_label};

pub(super) fn format_session_list_line(index: usize, entry: &SessionListEntry) -> String {
    let mut label = session_label(entry);
    if entry.archived {
        label.push_str(" [archived]");
    }
    let links = if entry.link_counts.is_empty() {
        String::new()
    } else {
        format!(
            "  pr:{} wt:{}",
            entry.link_counts.prs, entry.link_counts.worktrees
        )
    };
    let hidden = if entry.kind.is_user() {
        String::new()
    } else {
        format!("  [{}]", entry.kind)
    };
    format!(
        "  {:>2}. {}  {}  model={}  msgs={}{}{}  {}",
        index,
        entry.id,
        entry.updated_at.to_rfc3339(),
        entry.model,
        entry.message_count,
        links,
        hidden,
        label
    )
}

pub(super) fn format_session_detail(session: &Session) -> String {
    let console = session.console_path();
    // (label incl. padding, value); `None` rows are omitted.
    let rows: [(&str, Option<String>); 17] = [
        ("id:        ", Some(session.id.clone())),
        (
            "path:      ",
            Some(session.json_path().display().to_string()),
        ),
        (
            "console:   ",
            console.exists().then(|| console.display().to_string()),
        ),
        ("created:   ", Some(session.created_at.to_rfc3339())),
        ("updated:   ", Some(session.updated_at.to_rfc3339())),
        ("model:     ", Some(session.model.clone())),
        ("archived:  ", Some(session.archived.to_string())),
        ("auto_continue: ", Some(session.auto_continue.to_string())),
        ("thread:    ", Some(session.active_thread().id.clone())),
        ("threads:   ", Some(session.threads().len().to_string())),
        (
            "messages:  ",
            Some(
                session
                    .threads()
                    .iter()
                    .map(|thread| thread.messages.len())
                    .sum::<usize>()
                    .to_string(),
            ),
        ),
        ("kind:      ", Some(session.kind.to_string())),
        ("hidden:    ", Some(session.is_hidden().to_string())),
        ("parent:    ", session.parent_session_id.clone()),
        ("predecessor: ", session.predecessor_id.clone()),
        ("successor:   ", session.successor_id.clone()),
        (
            "title:     ",
            Some(
                session
                    .title
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .unwrap_or("(none)")
                    .to_string(),
            ),
        ),
    ];
    let mut out = String::new();
    for (label, value) in rows {
        if let Some(value) = value {
            out.push_str(&format!("{label}{value}\n"));
        }
    }
    if session.links.is_empty() {
        out.push_str("links:     (none)\n");
    } else {
        out.push_str(&format!("links:     ({})\n", session.links.len()));
        for (i, link) in session.links.iter().enumerate() {
            out.push_str(&format!("  [{i}] {}\n", format_link_one_line(link)));
        }
    }
    if session.scratchpad.is_empty() {
        out.push_str("scratchpad: (empty)\n");
    } else {
        out.push_str(&format!(
            "scratchpad: {} bytes\n---\n{}\n---\n",
            session.scratchpad.len(),
            session.scratchpad
        ));
    }
    out
}

pub(super) fn format_link_one_line(link: &SessionLink) -> String {
    match link {
        SessionLink::GitHubPr {
            url,
            repo,
            number,
            note,
        } => {
            let mut s = format!("pr {url}");
            if let (Some(r), Some(n)) = (repo, number) {
                s = format!("pr {r}#{n} ({url})");
            }
            if let Some(n) = note
                && !n.is_empty()
            {
                s.push_str(&format!(" — {n}"));
            }
            s
        }
        SessionLink::Worktree {
            host,
            path,
            branch,
            note,
        } => {
            let mut s = format!("worktree host={host} path={path}");
            if let Some(b) = branch
                && !b.is_empty()
            {
                s.push_str(&format!(" branch={b}"));
            }
            if let Some(n) = note
                && !n.is_empty()
            {
                s.push_str(&format!(" — {n}"));
            }
            s
        }
    }
}
