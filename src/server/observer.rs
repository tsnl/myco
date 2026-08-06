//! The observer surface: a person watching or driving a session's live
//! interactive state — bash shells on every host, and subagent children.
//! Reads are always open; writes obey the keyboard lock; every take,
//! handoff, opened terminal, and rename lands in the transcript as an
//! attributed, non-waking note through [`Server::accept_room_note`].

use std::sync::Arc;

use myco_api::Content;
use myco_api::{ApiError, ErrorKind};
use myco_api::{Author, Entry};

use myco_api as api;

use super::*;

use super::room::Accepted;

fn shell_of(host: String, s: crate::machines::tool_services::ShellOverview) -> api::Shell {
    api::Shell {
        host,
        id: s.id,
        title: s.title,
        cmdline: s.cmdline,
        running: s.running,
        exit_code: s.exit_code,
        lock: s.lock,
        end_offset: s.end_offset,
        pty: s.pty,
    }
}

/// Keystrokes as a person (or the agent) reads them back: printable text
/// untouched, Enter as a newline, everything else in caret notation (`^C`,
/// `^[[A`) — a transcript note is for reading, not replaying.
fn readable_keys(data: &str) -> String {
    let mut out = String::with_capacity(data.len());
    for c in data.chars() {
        match c {
            '\n' | '\r' => out.push('\n'),
            '\t' => out.push('\t'),
            '\x7f' => out.push_str("^?"),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push(char::from((c as u8) + 0x40));
            }
            c => out.push(c),
        }
    }
    out
}

/// A live child as the rail lists it. Metadata reads only — no snapshot.
fn subagent_of(l: &Live) -> api::Subagent {
    let (id, model) = l.session.with(|s| (s.id.clone(), s.model.clone()));
    api::Subagent {
        id,
        model,
        busy: l.is_busy(),
        lock: if l.user_holds() {
            api::ShellLockMode::User
        } else {
            api::ShellLockMode::Assistant
        },
    }
}

impl Server {
    /// Record a never-waking message through the room — shell keystrokes and
    /// keyboard-lock transitions. Same acceptance path as `post_message`
    /// (broadcast, inbox, poke, participants), so watchers see it at once and
    /// the agent reads it at its next boundary; forced non-waking because the
    /// user acted on the shell, not on the agent.
    fn accept_room_note(&self, live: &Arc<Live>, author: &Author, text: &str) {
        let entry = Entry::user(
            author.clone(),
            vec![Content::Text {
                text: text.to_string(),
            }],
        );
        let mut room = live.room.lock().unwrap_or_else(|e| e.into_inner());
        if let Author::User { id, .. } = author {
            room.participants.insert(id.clone());
        }
        let _ = live.events.send(SessionEvent::Message {
            entry: entry.clone(),
            wakes_agent: false,
        });
        room.inbox.push_back(Accepted {
            entry,
            wakes_agent: false,
        });
        let _ = live.tx.send(Cmd::Poke);
    }

    /// The live handle, or NotFound: the shell and subagent surfaces have
    /// nothing to say about a session whose agent task (and with it every
    /// bash session) is gone.
    async fn live_for_shells(&self, id: &str) -> Result<Arc<Live>, ApiError> {
        self.get_live(id)
            .await
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "session not live"))
    }

    pub(crate) async fn shells(&self, id: &str) -> Result<api::Shells, ApiError> {
        let shells = match self.get_live(id).await {
            Some(live) => live
                .harness()
                .shell_overviews()
                .await
                .into_iter()
                .map(|(host, s)| shell_of(host, s))
                .collect(),
            // Not resident is a fact, not a fault: the rail shows nothing.
            None => Vec::new(),
        };
        Ok(api::Shells { shells })
    }

    /// The controller for `host` on a live session's harness — every one-shell
    /// observer call resolves through this.
    async fn shell_host(
        &self,
        id: &str,
        host: &str,
    ) -> Result<(Arc<Live>, Arc<crate::machines::harness::HostController>), ApiError> {
        let live = self.live_for_shells(id).await?;
        let ctl = live
            .harness()
            .observer_host(host)
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        Ok((live, ctl))
    }

    pub(crate) async fn shell_tail(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        from: u64,
    ) -> Result<api::ShellTailChunk, ApiError> {
        /// One tail response's byte budget; a viewer further behind is
        /// skipped forward (the terminal wants the present).
        const TAIL_BUDGET: usize = 64 * 1024;
        let (_, ctl) = self.shell_host(id, host).await?;
        let tail = ctl
            .shell_tail(shell, from, TAIL_BUDGET)
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        Ok(api::ShellTailChunk {
            from: tail.from,
            end: tail.end,
            data: String::from_utf8_lossy(&tail.data).into_owned(),
            running: tail.running,
            lock: tail.lock,
        })
    }

    pub(crate) async fn shell_screen(
        &self,
        id: &str,
        host: &str,
        shell: &str,
    ) -> Result<api::ShellScreen, ApiError> {
        let (_, ctl) = self.shell_host(id, host).await?;
        // `api::ShellScreen` *is* `myco_types::ShellScreen` — one definition
        // serves the host protocol and the wire, so this is a passthrough.
        ctl.shell_screen(shell)
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))
    }

    /// Wait for the shell's output to move past `seen` (bounded by
    /// `timeout`); the shell WebSocket's pusher blocks here. Not on
    /// [`MycoApi`]: the trait is the request/response surface, and this is
    /// the socket's private half.
    pub(crate) async fn shell_wait_change(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        seen: u64,
        timeout: std::time::Duration,
    ) -> Result<u64, ApiError> {
        let (_, ctl) = self.shell_host(id, host).await?;
        ctl.shell_wait_change(shell, seen, timeout)
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))
    }

    pub(crate) async fn shell_resize(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        cols: u16,
        rows: u16,
    ) -> Result<api::Shell, ApiError> {
        let (_, ctl) = self.shell_host(id, host).await?;
        let overview = ctl
            .shell_resize(shell, cols, rows)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        Ok(shell_of(host.to_string(), overview))
    }

    /// Open a terminal for the user on `host`: a real bash session owned by
    /// this session's agent — its bash tool can write/read/signal/close it —
    /// starting user-held, because whoever opened it is the one typing.
    pub(crate) async fn shell_start(
        &self,
        author: &Author,
        id: &str,
        host: &str,
        req: api::CreateShell,
    ) -> Result<api::Shell, ApiError> {
        let (live, ctl) = self.shell_host(id, host).await?;
        let overview = ctl
            .shell_start(
                req.shell.as_deref(),
                live.agent_id,
                req.command.as_deref(),
                req.pty,
                req.cols,
                req.rows,
            )
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        // Announced like a keyboard grab: the agent can address this session
        // by name from here on, and must not wonder where it came from.
        self.accept_room_note(
            &live,
            author,
            &format!("[opened shell {:?} on {host} (user-held)]", overview.id),
        );
        Ok(shell_of(host.to_string(), overview))
    }

    /// Set or clear a shell's display name. The id stays the address; the
    /// note keeps the agent's mental map current.
    pub(crate) async fn shell_rename(
        &self,
        author: &Author,
        id: &str,
        host: &str,
        shell: &str,
        title: Option<String>,
    ) -> Result<api::Shell, ApiError> {
        let (live, ctl) = self.shell_host(id, host).await?;
        let overview = ctl
            .shell_rename(shell, title.as_deref())
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        let text = match &overview.title {
            Some(t) => format!("[named shell {shell:?} on {host} {t:?}]"),
            None => format!("[cleared shell {shell:?}'s name on {host}]"),
        };
        self.accept_room_note(&live, author, &text);
        Ok(shell_of(host.to_string(), overview))
    }

    pub(crate) async fn shell_input(
        &self,
        _author: &Author,
        id: &str,
        host: &str,
        shell: &str,
        data: String,
    ) -> Result<api::Shell, ApiError> {
        let (live, ctl) = self.shell_host(id, host).await?;
        let overview = ctl
            .shell_input(shell, &data)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        // The keystrokes become part of the conversation — the agent must not
        // discover a mutated shell with no explanation in its history — but
        // they stream one at a time now, so they accumulate per hold and
        // flush as a single note when the keyboard is handed back.
        live.typed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(format!("{host}/{shell}"))
            .or_default()
            .push_str(&data);
        Ok(shell_of(host.to_string(), overview))
    }

    pub(crate) async fn shell_lock(
        &self,
        author: &Author,
        id: &str,
        host: &str,
        shell: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Shell, ApiError> {
        let (live, ctl) = self.shell_host(id, host).await?;
        let (previous, overview) = ctl
            .shell_lock(shell, lock)
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        // Announce transitions only: a double-click that re-takes the
        // keyboard is not news.
        if previous != lock {
            match lock {
                api::ShellLockMode::User => {
                    // A fresh hold starts a fresh story.
                    live.typed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&format!("{host}/{shell}"));
                    self.accept_room_note(
                        &live,
                        author,
                        &format!("[took the keyboard for shell {shell:?} on {host}]"),
                    );
                }
                api::ShellLockMode::Assistant => {
                    let typed = live
                        .typed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&format!("{host}/{shell}"));
                    if let Some(data) = typed.filter(|d| !d.is_empty()) {
                        self.accept_room_note(
                            &live,
                            author,
                            &format!(
                                "[typed into shell {shell:?} on {host}]\n{}",
                                readable_keys(&data)
                            ),
                        );
                    }
                    self.accept_room_note(
                        &live,
                        author,
                        &format!("[returned the keyboard for shell {shell:?} on {host}]"),
                    );
                }
            }
        }
        Ok(shell_of(host.to_string(), overview))
    }

    /// The live children of `parent` — subagent sessions whose agent tasks
    /// exist right now. The rail's list, so it must stay cheap: metadata
    /// reads under the session lock, never a snapshot of the entries.
    async fn live_children(&self, parent: &str) -> Vec<Arc<Live>> {
        let map = self.live.lock().await;
        let mut children: Vec<Arc<Live>> = map
            .values()
            .filter(|l| {
                l.session.with(|s| {
                    s.kind == crate::session::SessionKind::Subagent
                        && s.parent_session_id.as_deref() == Some(parent)
                })
            })
            .cloned()
            .collect();
        children.sort_by_key(|l| l.session.with(|s| s.created_at));
        children
    }

    /// One live child of `parent`, or NotFound — the subagent surface has
    /// nothing to say about children whose agent tasks are gone.
    async fn live_child(&self, parent: &str, child: &str) -> Result<Arc<Live>, ApiError> {
        self.live_children(parent)
            .await
            .into_iter()
            .find(|l| l.session.with(|s| s.id == child))
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such live subagent"))
    }

    pub(crate) async fn subagents(&self, id: &str) -> Result<api::Subagents, ApiError> {
        // Not resident is a fact, not a fault, same as the shells rail — and
        // children die with their own tasks, not the parent's.
        let subagents = self
            .live_children(id)
            .await
            .iter()
            .map(|l| subagent_of(l))
            .collect();
        Ok(api::Subagents { subagents })
    }

    pub(crate) async fn subagent_lock(
        &self,
        author: &Author,
        id: &str,
        child: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Subagent, ApiError> {
        let parent = self.live_for_shells(id).await?;
        let child_live = self.live_child(id, child).await?;
        let wanted = lock == api::ShellLockMode::User;
        let previous = child_live.set_user_hold(wanted);
        // Announce transitions only, in the parent's transcript — the room
        // where the agent that owns this child lives.
        if previous != wanted {
            let short = &child[..child.len().min(8)];
            let text = if wanted {
                format!("[took subagent {short}]")
            } else {
                format!("[handed subagent {short} back to the agent]")
            };
            self.accept_room_note(&parent, author, &text);
        }
        Ok(subagent_of(&child_live))
    }

    pub(crate) async fn subagent_input(
        &self,
        author: &Author,
        id: &str,
        child: &str,
        text: String,
    ) -> Result<api::Subagent, ApiError> {
        let parent = self.live_for_shells(id).await?;
        let child_live = self.live_child(id, child).await?;
        if !child_live.user_holds() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "subagent is agent-held; take it before posting into it",
            ));
        }
        let child_id = child_live.session.id();
        self.post_message(author, &child_id, api::PostMessage { text: text.clone() })
            .await?;
        // Mirror shell keystrokes: what was said to the child is part of the
        // parent's conversation too — the parent agent must not continue a
        // child that answered someone it never saw ask.
        let short = &child[..child.len().min(8)];
        self.accept_room_note(
            &parent,
            author,
            &format!("[posted to subagent {short}]\n{text}"),
        );
        Ok(subagent_of(&child_live))
    }
}
