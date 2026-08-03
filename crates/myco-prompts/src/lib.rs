//! Shared system-prompt fragments for myco agents.
//!
//! Always-on agent policy (worktrees, computer-use, coding norms, user
//! authority, the agent workspace) lives here. Longer runtime docs live in
//! [`crate::manual`], exported to disk at startup, and pointed at from the
//! `# Manual` section below.

pub mod manual;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, SecondsFormat, Utc};

/// Epilogue appended to every agent system prompt.
pub const DEFAULT_AGENT_PROMPT_EPILOGUE: &str = concat!(
    r#"
When generating a response, please follow these guidelines.

Note that this section onward (the Myco Agent Prompt Epilogue) is made available to every myco
agent in the system prompt, including nested ones.

---

# Myco Runtime Manual

You are running inside **myco**: a mycelial agent runtime. The same agent pattern repeats at every
scale — supervisors drive **nested myco agents** over the local API (see Nested Agents below),
and tools run on **hosts** (hands) spanning local and remote machines. The **local** host is always
enabled **in-process** (no subprocess). Remotes use `ssh … myco --mode host` over NDJSON. Local
tools (`session_meta`) stay in the agent process; host tools (`bash`, editor) run on
a host worker (local in-process or remote).

**Runtime docs are markdown files on disk** — see *Manual* below for the directory this build
exported them to. Read and search them with the tools you already have (`rg`, the editor);
`index.md` names the articles: `overview.md`, `api.md`, `harness-ops.md`.

Quick map (details in the manual):
- Hosts: every concrete `Host` alias in `~/.ssh/config` is a remote host (`Include`s followed);
  local is always on. `~/.myco/config.toml` (or `$MYCO_CONFIG`) holds knobs only
  (`attach_timeout_secs`, `max_soul_bytes`).
- Sessions: `~/.myco/v2/session/{shard}/{id}.json` — use `session_meta`, not raw file edits.
- Host tools take optional `host`; omitted → **`local`** (in-process). Remotes are lazy on first use.
- **To act on a remote machine, set `host` on the tool call — do not run `ssh <alias> …` from
  local `bash`.** A host worker keeps one persistent SSH connection, so each call skips connection
  setup, and you get real bash sessions, the editor, and search on that machine. Shell out to
  `ssh`/`scp` only for what a host worker cannot do itself: diagnosing a DOWN host, installing
  `myco` there, or moving files between machines.
- `bash`: prefer optional `cwd` on `exec`/`start` over `cd … &&` (leading `cd` in `command` is rejected).
- Text search: use `bash` + `rg`/`grep` (`rg` for code trees; `grep -r` as fallback). For
  search **by meaning**, use `ck` where installed (`ck --sem "query" dir/`; hybrid BM25 +
  semantic, persistent per-folder index, `cargo install ck-search`) — probe with
  `command -v ck`. Project guidance lives in `AGENTS.md`/`CLAUDE.md` and skill packs
  (`.claude/skills`, `SKILL.md` folders) — read them with the editor or `rg` when the task
  touches how this project works.
- You cannot run slash-commands (`/hosts`, `/session`, …); tell the user which to run.
- Updating `myco` on **remote** hosts: compile **on the target** (see `harness-ops.md`).
  If developing myco, archive the local git tree; else download a source snapshot from
  https://github.com/tsnl/myco/releases (match `session_meta` `executable_path` +
  `myco --version`). Never scp prebuilt binaries across machines (glibc/arch mismatch).

---

# Nested Agents

Context is precious. For ephemeral, task-specific context — and for complex, multi-step tasks —
delegate to a nested agent with the `subagent` tool: one call runs one full turn of a hidden
child session and returns its final answer.

Nesting happens **on the local host only** (the tool is root-only by construction) and entirely
server-side, so it works even when machines sit behind strict firewalls. The brain stays on this
machine — model access, config, keys, and the session store are shared — and a nested agent
reaches remote machines through the same host pool as you. Remote hosts stay hands, not brains:
they need only `myco` on PATH plus SSH, never config or keys.

Usage: call `subagent` with a self-contained prompt per turn; `model: <key>` picks a model.
Context forking: add `fork: true` to seed a fresh child with a copy of your saved conversation
instead of a blank context. Fork when the task needs what you already know (decisions so far,
investigation, the user's intent); start blank when the task is self-contained — a fork begins at
your context size and has less headroom. Launch forks on your own model (the catalog key stamped
at the end of this prompt): a same-model fork's first request re-reads your cached prompt prefix
at cache rates. The result starts
with `session: <id>` — pass that id back as `session_id` to continue the same child with
follow-up turns. Ask for terse summaries. Children are hidden (`kind: subagent`, parented to
yours automatically — your own session id is on the newest `# Session` block in this
conversation) in the shared `~/.myco/v2/session/` store — read them later via `session_meta` get-by-id or
`list` with `include_hidden: true`.

The same surface is plain HTTP for scripts: the server exports `$MYCO_API`; POST
`$MYCO_API/sessions` (`{parent_session, fork?, model?}`) and `$MYCO_API/sessions/<id>/messages`
(`{text}`), then poll `$MYCO_API/sessions/<id>/poll?since=<n>` until `busy` is false.

---
"#,
    include_str!("fragments/worktrees.md"),
    "\n---\n\n",
    include_str!("fragments/computer-use.md"),
    "\n---\n\n",
    include_str!("fragments/coding-norms.md"),
    "\n---\n\n",
    include_str!("fragments/user-authority.md"),
    "\n---\n\n",
    include_str!("fragments/workspace.md"),
    "\n",
);

/// Stamp appended after the epilogue (and soul) naming the running model's
/// catalog key, so agents can spawn nested/forked children on the same model.
///
/// Keep this identity-free: the model key is shared by a supervisor and its
/// cache-aligned forks, but any per-process value (session id, agent id) or
/// mid-session-mutable value (effort) here would change the system-prompt
/// bytes per agent and break fork prompt-cache reuse from the first byte.
pub fn model_stamp(model_key: &str) -> String {
    format!(
        "---\n\n# Current Model\n\nCatalog key: `{model_key}` — pass `--model {model_key}` when \
         spawning nested or forked myco agents to keep them on this model.\n"
    )
}

/// Heading of the block [`session_stamp`] builds, and the marker that tells a
/// stamp apart from something a user typed.
const SESSION_STAMP_HEADING: &str = "# Session";

/// Where this agent is running, as a block for the **first user message** of a
/// conversation: the session id `--resume` / `--parent-session` take, when the
/// session began, and the directory myco was launched in — the facts an agent
/// would otherwise spend a `session_meta` round trip and a `pwd` to learn.
///
/// It rides a message rather than the system prompt for the reason
/// [`model_stamp`] documents: the prompt must stay byte-identical across an
/// agent and its forks, and every field here is per-process. A fork inherits
/// the parent's stamped first message, so it stamps its own onto the first
/// message it adds — hence "from here on", and hence the newest block wins.
///
/// `started_at` is the session's creation time, so the line is honest about
/// being a start rather than a clock: a session open for days would otherwise
/// carry a confidently wrong "now" in its prompt, and `date` is always right.
pub fn session_stamp(session_id: &str, started_at: DateTime<Utc>) -> String {
    stamp_with(
        session_id,
        started_at,
        std::env::current_dir().ok().as_deref(),
    )
}

/// [`session_stamp`] against an explicit launch directory, so tests need no
/// process-global cwd override.
fn stamp_with(session_id: &str, started_at: DateTime<Utc>, cwd: Option<&Path>) -> String {
    let started = started_at
        .with_timezone(&Local)
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut block = format!(
        "{SESSION_STAMP_HEADING}\n\n- Session id: `{session_id}` — this conversation from here \
         on. Nested agents (the `subagent` tool) are parented to it automatically; `session_meta` \
         action=get has the rest of this session's metadata.\n- Started: {started} — when this \
         session began, not the current time; run `date` for that.\n"
    );
    if let Some(cwd) = cwd {
        block.push_str(&format!(
            "- Launch directory: `{}` — where myco was started, so `bash` on the local host \
             begins there unless a call passes `cwd`.\n",
            cwd.display()
        ));
    }
    block
}

/// Whether a user-message text block is a [`session_stamp`] rather than the
/// user's own words. Session labels and search snippets read the first user
/// message, and the stamp is myco's payload, not something anyone typed.
pub fn is_session_stamp(text: &str) -> bool {
    text.starts_with(SESSION_STAMP_HEADING)
}

/// Cap used when `config.toml` sets no `max_soul_bytes`.
///
/// Backstop so one runaway soul revision cannot bloat every future prompt
/// (the fragment asks for about a screenful; same cap as the session
/// scratchpad). The truncation marker tells the agent to write a shorter one,
/// and startup warns the user ([`soul_truncation`]).
pub const DEFAULT_MAX_SOUL_BYTES: usize = 64 * 1024;

/// Same backstop for injected project guidance (`AGENTS.md` / `CLAUDE.md`).
/// Not user-configurable: `max_soul_bytes` is named for the soul and sizing
/// the two together would surprise anyone who lowers it to shorten the soul.
const MAX_GUIDANCE_BYTES: usize = 64 * 1024;

/// A soul version that did not fit under the `max_soul_bytes` cap. Losing the
/// tail of the soul is silent from inside the prompt, so this is surfaced two
/// ways: a marker in the prompt itself and a startup WARNING for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoulTruncation {
    /// Filename of the truncated version, under `workspace/soul/`.
    pub version: String,
    /// Size of the trimmed version on disk.
    pub bytes: usize,
    /// The `max_soul_bytes` cap it was cut to.
    pub limit: usize,
}

impl SoulTruncation {
    /// One line naming the cut, shared by the in-prompt marker and the
    /// startup warning so both report the same numbers.
    pub fn describe(&self) -> String {
        format!(
            "soul truncated at {} of {} (max_soul_bytes)",
            self.human_limit(),
            human_bytes(self.bytes)
        )
    }

    /// The cap as a size for humans, e.g. `64 KiB`.
    pub fn human_limit(&self) -> String {
        human_bytes(self.limit)
    }
}

/// Byte counts for humans: `64 KiB`, `128.9 KiB`, `512 B`.
fn human_bytes(n: usize) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let kib = n as f64 / 1024.0;
    if (kib.round() - kib).abs() < 0.05 {
        format!("{} KiB", kib.round() as usize)
    } else {
        format!("{kib:.1} KiB")
    }
}

/// The epilogue plus the current soul (`~/.myco/v2/workspace/soul/`, respecting
/// `MYCO_HOME`, capped at `max_soul_bytes`), the launch directory's project
/// guidance (`AGENTS.md` / `CLAUDE.md`), and a listing of the rest of the
/// workspace, when present. Read at model build time — session start, model
/// switch, each worker spawn — so a running agent's prompt never changes
/// mid-conversation and the cached conversation prefix stays valid.
pub fn agent_prompt_epilogue(max_soul_bytes: usize) -> String {
    epilogue_with(
        myco_core::data_root().ok(),
        std::env::current_dir().ok(),
        max_soul_bytes,
    )
}

/// Whether the soul the next agent prompt will carry is cut short by the
/// `max_soul_bytes` cap. Startup calls this to warn before the first turn;
/// `None` means the soul fits (or there is none).
pub fn soul_truncation(max_soul_bytes: usize) -> Option<SoulTruncation> {
    let home = myco_core::data_root().ok()?;
    let dir = home.join("workspace").join("soul");
    capped_soul(&dir, max_soul_bytes)?.2
}

/// The current soul snapshot: filename and trimmed contents of the
/// lexicographically last visible `*.md` in `workspace/soul/`. Versions are
/// write-once maildir-style files, so "newest name wins" is the whole
/// contract — a whitespace-only newest version reads as "no soul".
fn latest_soul(dir: &Path) -> Option<(String, String)> {
    let mut versions: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            (!name.starts_with('.') && name.ends_with(".md") && entry.path().is_file())
                .then(|| (name, entry.path()))
        })
        .collect();
    versions.sort();
    let (name, path) = versions.pop()?;
    let text = std::fs::read_to_string(path).ok()?.trim().to_string();
    (!text.is_empty()).then_some((name, text))
}

/// Project guidance for the launch directory: `AGENTS.md` (preferred) or
/// `CLAUDE.md`, when present and non-empty. Injected at session start so the
/// agent knows how this project works without any indexing machinery.
fn project_guidance(dir: &std::path::Path) -> Option<(String, String)> {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            let text = text.trim().to_string();
            if !text.is_empty() {
                return Some((name.to_string(), text));
            }
        }
    }
    None
}

/// The current soul as it goes into a prompt: version name, text cut to
/// `max_bytes` (on a char boundary, with a marker naming the cut), and the
/// truncation record when the on-disk version did not fit.
fn capped_soul(
    dir: &std::path::Path,
    max_bytes: usize,
) -> Option<(String, String, Option<SoulTruncation>)> {
    let (version, mut text) = latest_soul(dir)?;
    let cut = SoulTruncation {
        version: version.clone(),
        bytes: text.len(),
        limit: max_bytes,
    };
    let marker = format!("\n\n[{} — write a shorter revision]", cut.describe());
    let truncated = cap_bytes(&mut text, max_bytes, &marker);
    Some((version, text, truncated.then_some(cut)))
}

/// Truncate to `max` bytes on a char boundary, appending `marker` when cut.
/// Returns whether anything was cut, so callers can report it.
fn cap_bytes(text: &mut String, max: usize, marker: &str) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(marker);
    true
}

/// [`agent_prompt_epilogue`] against explicit dirs, so tests need no
/// process-global `MYCO_HOME` / cwd override.
///
/// Blocks are ordered least to most volatile, because every agent's prompt
/// carries them as a prefix: the manual path changes only when the binary
/// does, the soul only on a deliberate revision, project guidance only when
/// the repo's own file does, while any new workspace file rewrites the
/// listing. Keeping the churniest block last leaves the longest shared prefix
/// for same-model forks to hit in the prompt cache.
fn epilogue_with(
    home: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    max_soul_bytes: usize,
) -> String {
    let mut prompt = DEFAULT_AGENT_PROMPT_EPILOGUE.to_string();
    if let Some(home) = home.as_deref() {
        prompt.push_str(&manual_section(&crate::manual::dir(home)));
    }
    let workspace = home.map(|home| home.join("workspace"));
    let soul = workspace
        .as_deref()
        .and_then(|ws| capped_soul(&ws.join("soul"), max_soul_bytes));
    if let Some((name, soul, _)) = soul {
        prompt.push_str(&format!(
            "\n---\n\n# Soul\n\n(current version: soul/{name})\n\n{soul}\n"
        ));
    }
    if let Some((name, mut guidance)) = cwd.as_deref().and_then(project_guidance) {
        cap_bytes(
            &mut guidance,
            MAX_GUIDANCE_BYTES,
            "\n\n[project guidance truncated at 64 KiB]",
        );
        prompt.push_str(&format!(
            "\n---\n\n# Project guidance ({name})\n\n{guidance}\n"
        ));
    }
    if let Some(listing) = workspace.as_deref().and_then(workspace_listing) {
        prompt.push_str(&listing);
    }
    prompt
}

/// Where startup exported this build's manual ([`crate::manual::export`]).
/// The directory is named rather than quoted: the articles run to tens of
/// kilobytes, and an agent that can `rg` them does not need them in context.
fn manual_section(dir: &Path) -> String {
    format!(
        "\n---\n\n# Manual\n\nRuntime docs for this myco build: `{}` (`index.md` plus one file \
         per article). Search them like any other files — `rg -n 'ControlMaster' {}`. They are \
         this binary's own docs, refreshed at startup, so trust them over memory when host, \
         install, or config behavior is unclear.\n",
        dir.display(),
        dir.display(),
    )
}

/// Bounds on the workspace listing. A workspace can hold anything, so the
/// prompt cost is capped here rather than by asking the agent to delete notes.
const MAX_LISTING_BYTES: usize = 8 * 1024;
const MAX_LISTING_ENTRIES: usize = 200;
/// Nested layout is the agent's business, but an unbounded walk is not.
const MAX_LISTING_DEPTH: u32 = 4;
/// Files stat'd before the walk gives up, so a huge tree cannot stall startup.
const MAX_LISTING_SCAN: usize = 4_000;
/// Enough of each file to reach its first heading without reading the body.
const TITLE_PROBE_BYTES: usize = 512;
const MAX_TITLE_CHARS: usize = 80;

/// A `# Workspace Files` section: one line per workspace file, giving its path
/// relative to `workspace/`, the UTC day it last changed, and its title.
///
/// This is the read side of the workspace. A file the agent has forgotten is a
/// file it will not open, so the listing makes existence free to check while
/// leaving contents to a deliberate read.
///
/// Two choices keep the block cache-stable, since it lands in every agent's
/// prompt prefix: days rather than timestamps (repeated writes to a file
/// already listed as today change nothing), and path order rather than recency
/// (touching one file cannot reshuffle the block). `soul/` is skipped — the
/// live version is already quoted above, and its superseded siblings are noise.
fn workspace_listing(workspace: &Path) -> Option<String> {
    let mut entries = Vec::new();
    collect_listing(workspace, workspace, 0, &mut entries);
    if entries.is_empty() {
        return None;
    }
    entries.sort();

    let total = entries.len();
    let mut body = String::new();
    let mut shown = 0usize;
    for (path, day, title) in entries {
        let line = match title {
            Some(title) => format!("- `{path}` — {day} — {title}\n"),
            None => format!("- `{path}` — {day}\n"),
        };
        if shown == MAX_LISTING_ENTRIES || body.len() + line.len() > MAX_LISTING_BYTES {
            break;
        }
        body.push_str(&line);
        shown += 1;
    }
    // "at least", because the walk itself stops at MAX_LISTING_SCAN.
    if shown < total {
        body.push_str(&format!(
            "\n[at least {} more file(s) not listed — consolidate the workspace]\n",
            total - shown
        ));
    }

    Some(format!(
        "\n---\n\n# Workspace Files\n\nUnder `~/.myco/workspace/` — path, UTC day \
         last changed, title. A listing, not the contents: read the files that \
         touch your task.\n\n{body}"
    ))
}

/// Walk `dir`, pushing `(relative path, day, title)` per visible regular file.
/// Hidden names are skipped (in-progress writes use them) and symlinks are
/// never followed, matching skill discovery.
fn collect_listing(
    root: &Path,
    dir: &Path,
    depth: u32,
    out: &mut Vec<(String, String, Option<String>)>,
) {
    if depth > MAX_LISTING_DEPTH || out.len() >= MAX_LISTING_SCAN {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };

    let mut subdirs = Vec::new();
    for entry in read.flatten() {
        // Also inside the loop: one flat directory can hold more files than the
        // whole scan budget, and each costs a stat plus a title probe.
        if out.len() >= MAX_LISTING_SCAN {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if name.starts_with('.') || file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if depth == 0 && name == "soul" {
                continue;
            }
            subdirs.push(entry.path());
        } else if file_type.is_file() {
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let day = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .map(|at| DateTime::<Utc>::from(at).format("%Y-%m-%d").to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            out.push((
                relative.to_string_lossy().to_string(),
                day,
                file_title(&path),
            ));
        }
    }

    for sub in subdirs {
        collect_listing(root, &sub, depth + 1, out);
    }
}

/// First markdown heading in the file's opening bytes, else its first
/// non-empty line. `None` when the head holds neither (binaries, blank files).
fn file_title(path: &Path) -> Option<String> {
    use std::io::Read;

    let mut head = vec![0u8; TITLE_PROBE_BYTES];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut head).ok()?;
    head.truncate(read);
    // A NUL in the head means binary; nothing quotable, and a lossy decode
    // would spray control bytes through every agent's prompt.
    if head.contains(&0) {
        return None;
    }
    let head = String::from_utf8_lossy(&head);

    let mut first_line = None;
    for line in head.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(heading) = line.strip_prefix('#') {
            if let Some(title) = clean_title(heading.trim_start_matches('#').trim()) {
                return Some(title);
            }
            continue;
        }
        first_line = first_line.or(Some(line));
    }
    first_line.and_then(clean_title)
}

/// Title with control characters dropped and length capped. `None` when
/// nothing printable survives, so the line renders as path and day alone.
fn clean_title(title: &str) -> Option<String> {
    let printable: String = title.chars().filter(|c| !c.is_control()).collect();
    let printable = printable.trim();
    if printable.is_empty() {
        return None;
    }
    let mut clamped: String = printable.chars().take(MAX_TITLE_CHARS).collect();
    if clamped.chars().count() < printable.chars().count() {
        clamped.push('…');
    }
    Some(clamped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use myco_test_support::{TempDir, temp_dir};

    /// Temp home-shaped dir with `workspace/soul/` created — the layout the
    /// soul/workspace fixtures build on. Panic-safe cleanup via [`TempDir`].
    fn soul_home(tag: &str) -> TempDir {
        let home = temp_dir(tag);
        std::fs::create_dir_all(home.path().join("workspace").join("soul")).unwrap();
        home
    }

    #[test]
    fn epilogue_includes_always_on_policy() {
        for needle in [
            "Git worktrees for new features",
            "Computer use",
            "Think Before Coding",
            "User authority & privileged operations",
            "force-merge",
            "manual",
            // Nested-agent recipe goes through the local API.
            "self-contained prompt",
            "$MYCO_API/sessions",
            // Remote work goes through the `host` field, not local `ssh`.
            "do not run `ssh <alias> …` from",
            "persistent SSH connection",
            // runtime catalog pointer, not full policy-as-articles
            "`harness-ops.md`",
            // Search guidance is bash-first; myco ships no search tools of
            // its own. Semantic search is the external `ck` companion.
            "`rg`",
            "`ck`",
            // Free-form workspace policy: maildir-style soul versions, the
            // recall/record habit, and the consistency caution.
            "Workspace & soul",
            "~/.myco/v2/workspace/soul/",
            "write-once, never edited in place",
            "weakly consistent",
        ] {
            assert!(
                DEFAULT_AGENT_PROMPT_EPILOGUE.contains(needle),
                "missing: {needle}"
            );
        }
        // #98 moved the manual to disk: the epilogue points at files to
        // search, and must not advertise a `manual` tool any more.
        assert!(!DEFAULT_AGENT_PROMPT_EPILOGUE.contains("`manual` tool"));
        assert!(!DEFAULT_AGENT_PROMPT_EPILOGUE.contains("indexed_exact_text_search"));
    }

    /// Project guidance from the launch directory is appended at model build
    /// time — AGENTS.md preferred, CLAUDE.md fallback, absent = no section.
    #[test]
    fn project_guidance_is_appended_from_cwd() {
        let cwd = temp_dir("guidance");
        let dir = cwd.path().to_path_buf();
        let epilogue = || epilogue_with(None, Some(dir.clone()), DEFAULT_MAX_SOUL_BYTES);

        assert_eq!(epilogue(), DEFAULT_AGENT_PROMPT_EPILOGUE);

        std::fs::write(dir.join("CLAUDE.md"), "claude_guidance_token\n").unwrap();
        let prompt = epilogue();
        assert!(
            prompt.contains("# Project guidance (CLAUDE.md)"),
            "{prompt}"
        );
        assert!(prompt.contains("claude_guidance_token"), "{prompt}");

        // AGENTS.md wins over CLAUDE.md when both exist.
        std::fs::write(dir.join("AGENTS.md"), "agents_guidance_token\n").unwrap();
        let prompt = epilogue();
        assert!(
            prompt.contains("# Project guidance (AGENTS.md)"),
            "{prompt}"
        );
        assert!(prompt.contains("agents_guidance_token"), "{prompt}");
        assert!(!prompt.contains("claude_guidance_token"), "{prompt}");
    }

    /// The manual is not a tool any more, so the prompt has to carry the
    /// resolved path — an agent told only `~/.myco` would guess wrong under
    /// `MYCO_HOME`, and a stale build's directory is a different one.
    #[test]
    fn manual_path_in_the_prompt_follows_myco_home() {
        let home =
            std::env::temp_dir().join(format!("myco-manual-prompt-{}", uuid::Uuid::new_v4()));
        let prompt = epilogue_with(Some(home.clone()), None, DEFAULT_MAX_SOUL_BYTES);
        let dir = crate::manual::dir(&home);
        assert!(prompt.contains("# Manual"), "{prompt}");
        assert!(prompt.contains(&dir.display().to_string()), "{prompt}");
        assert!(prompt.contains("index.md"), "{prompt}");

        // No home to resolve: no path claimed rather than a guessed one.
        let blind = epilogue_with(None, None, DEFAULT_MAX_SOUL_BYTES);
        assert!(!blind.contains("# Manual"), "{blind}");
    }

    /// The soul is prompt-resident, so the fragment has to say what earns a
    /// place there (distilled dated pointers), what keeps it honest (prune on
    /// every add, evidence over prompt), and that the listing exists to read
    /// from.
    #[test]
    fn workspace_fragment_biases_toward_recording_and_pruning() {
        for needle in [
            "index over the workspace",
            "distilled line and the pointer",
            "Date every line",
            "Every revision that adds also prunes",
            "Evidence beats the prompt",
            "`# Workspace Files` section",
            "read the listed files",
        ] {
            assert!(
                DEFAULT_AGENT_PROMPT_EPILOGUE.contains(needle),
                "missing: {needle}"
            );
        }
    }

    #[test]
    fn fork_recipe_and_model_stamp_are_documented() {
        // The epilogue points at the stamp; the stamp names the key and flag.
        for needle in ["Context forking", "fork: true", "at the end of this prompt"] {
            assert!(
                DEFAULT_AGENT_PROMPT_EPILOGUE.contains(needle),
                "missing: {needle}"
            );
        }
        let stamp = model_stamp("grok-4");
        assert!(stamp.contains("# Current Model"), "{stamp}");
        assert!(stamp.contains("`grok-4`"), "{stamp}");
        assert!(stamp.contains("--model grok-4"), "{stamp}");
    }

    /// Where the agent is running reaches it through the conversation, never
    /// the system prompt: a per-process value there would change the prompt
    /// bytes per agent and break fork prompt-cache reuse from the first byte.
    #[test]
    fn session_stamp_names_the_session_and_stays_out_of_the_system_prompt() {
        let id = "cafef00dcafef00dcafef00dcafef00d";
        let started_at = DateTime::parse_from_rfc3339("2026-07-29T14:02:11Z")
            .unwrap()
            .with_timezone(&Utc);
        let stamp = stamp_with(id, started_at, Some(Path::new("/home/user/myco")));

        assert!(stamp.contains(&format!("`{id}`")), "{stamp}");
        assert!(stamp.contains("`subagent`"), "{stamp}");
        assert!(stamp.contains("`/home/user/myco`"), "{stamp}");
        assert!(is_session_stamp(&stamp), "{stamp}");
        assert!(!is_session_stamp("please compact the session"));

        // The rendered time is the session's start instant, whatever zone the
        // machine renders it in, and it says so — a session open for days must
        // not read as a clock.
        let rendered = stamp
            .lines()
            .find_map(|l| l.trim().strip_prefix("- Started: "))
            .expect("a Started line");
        let (time, note) = rendered.split_once(' ').expect("time then note");
        assert_eq!(
            DateTime::parse_from_rfc3339(time)
                .unwrap()
                .with_timezone(&Utc),
            started_at,
            "{stamp}"
        );
        assert!(note.contains("not the current time"), "{stamp}");

        // A launch directory myco cannot read drops the line rather than
        // guessing at one.
        assert!(!stamp_with(id, started_at, None).contains("Launch directory"));

        let prompt = epilogue_with(None, None, DEFAULT_MAX_SOUL_BYTES);
        assert!(!prompt.contains(id), "{prompt}");
        // The epilogue points agents at the stamp instead of a tool call.
        assert!(prompt.contains("newest `# Session` block"), "{prompt}");
    }

    #[test]
    fn newest_soul_version_is_appended_to_the_epilogue() {
        let home = soul_home("soul");
        let dir = home.path().to_path_buf();
        let soul_dir = dir.join("workspace").join("soul");
        let epilogue = || epilogue_with(Some(dir.clone()), None, DEFAULT_MAX_SOUL_BYTES);

        // No versions: the epilogue plus the unconditional Manual block, and
        // nothing else.
        let base = format!(
            "{DEFAULT_AGENT_PROMPT_EPILOGUE}{}",
            manual_section(&crate::manual::dir(&dir))
        );
        assert_eq!(epilogue(), base);

        // One version: appended verbatim under the promised heading, with the
        // live version named so agents know what to supersede.
        std::fs::write(soul_dir.join("20260101T0000-aaaa.md"), "soul_token_alpha\n").unwrap();
        let prompt = epilogue();
        assert!(
            prompt.starts_with(DEFAULT_AGENT_PROMPT_EPILOGUE),
            "{prompt}"
        );
        assert!(prompt.contains("# Soul"), "{prompt}");
        assert!(
            prompt.contains("(current version: soul/20260101T0000-aaaa.md)"),
            "{prompt}"
        );
        assert!(prompt.ends_with("soul_token_alpha\n"), "{prompt}");

        // The lexicographically last name wins; hidden temp files and non-md
        // files are ignored (in-progress writes never leak into prompts).
        std::fs::write(soul_dir.join("20270101T0000-bbbb.md"), "soul_token_beta\n").unwrap();
        std::fs::write(soul_dir.join(".tmp-20280101T0000.md"), "tmp_token_gamma\n").unwrap();
        std::fs::write(soul_dir.join("zz-notes.txt"), "txt_token_delta\n").unwrap();
        let prompt = epilogue();
        assert!(prompt.contains("soul_token_beta"), "{prompt}");
        assert!(!prompt.contains("soul_token_alpha"), "{prompt}");
        assert!(!prompt.contains("tmp_token_gamma"), "{prompt}");
        assert!(!prompt.contains("txt_token_delta"), "{prompt}");

        // A whitespace-only newest version reads as a cleared soul — no
        // fallback to older versions.
        std::fs::write(soul_dir.join("20280101T0000-cccc.md"), "  \n\n").unwrap();
        assert_eq!(epilogue(), base);

        // An oversized version is truncated with a visible marker, keeping
        // the prompt bounded no matter what got written.
        std::fs::write(
            soul_dir.join("20290101T0000-dddd.md"),
            "x".repeat(DEFAULT_MAX_SOUL_BYTES * 2),
        )
        .unwrap();
        let prompt = epilogue();
        assert!(prompt.contains("[soul truncated at 64 KiB"), "{prompt}");
        // `base` (not the bare const): since #98 the epilogue also carries the
        // unconditional Manual block naming the exported directory.
        assert!(prompt.len() < base.len() + DEFAULT_MAX_SOUL_BYTES + 200);
    }

    #[test]
    fn the_cap_is_configurable_and_reports_what_it_cut() {
        let home = soul_home("soul-cap");
        let soul_dir = home.path().join("workspace").join("soul");
        // Multi-byte tail: the cut lands on a char boundary, never mid-char.
        let body = "é".repeat(1000);
        std::fs::write(soul_dir.join("20260101T0000-aaaa.md"), &body).unwrap();

        // A cap above the version's size leaves it verbatim and unreported.
        let (_, text, cut) = capped_soul(&soul_dir, DEFAULT_MAX_SOUL_BYTES).unwrap();
        assert_eq!(text, body);
        assert_eq!(cut, None);

        // A tighter cap from config.toml applies instead of the default.
        let (_, text, cut) = capped_soul(&soul_dir, 501).unwrap();
        let cut = cut.expect("501 < 2000 bytes should truncate");
        assert_eq!(cut.version, "20260101T0000-aaaa.md");
        assert_eq!((cut.bytes, cut.limit), (2000, 501));
        // 501 is mid-`é`, so the text is cut back to 500 plus the marker.
        assert!(text.starts_with(&"é".repeat(250)), "{text}");
        assert!(text.contains("[soul truncated at 501 B of 2 KiB"), "{text}");
        assert!(text.contains("write a shorter revision"), "{text}");
    }

    #[test]
    fn lowering_the_soul_cap_leaves_project_guidance_alone() {
        // `max_soul_bytes` is named for the soul; shrinking it to shorten a
        // soul must not silently start cutting AGENTS.md too.
        let home = soul_home("both");
        let dir = home.path().to_path_buf();
        let soul_dir = dir.join("workspace").join("soul");
        std::fs::write(soul_dir.join("20260101T0000-aaaa.md"), "s".repeat(4000)).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "g".repeat(4000)).unwrap();

        let prompt = epilogue_with(Some(dir.clone()), Some(dir.clone()), 1024);
        assert!(
            prompt.contains("[soul truncated at 1 KiB of 3.9 KiB"),
            "{prompt}"
        );
        assert!(!prompt.contains("[project guidance truncated"), "{prompt}");
        assert!(prompt.contains(&"g".repeat(4000)), "{prompt}");
    }

    #[test]
    fn workspace_listing_names_visible_files_after_the_soul() {
        let home = soul_home("listing");
        let dir = home.path().to_path_buf();
        let workspace = dir.join("workspace");
        let soul_dir = workspace.join("soul");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();

        std::fs::write(soul_dir.join("20260101T0000-aaaa.md"), "soul_token_alpha\n").unwrap();
        std::fs::write(
            workspace.join("notes").join("devbox.md"),
            "# Devbox build gotchas\n\nthe long material\n",
        )
        .unwrap();
        std::fs::write(workspace.join("scratch.txt"), "\n\nfirst line wins\n").unwrap();
        std::fs::write(workspace.join(".tmp-draft.md"), "# hidden_token\n").unwrap();
        std::fs::write(workspace.join("index.bin"), b"\x00\x01binary_token\n").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "agents_guidance_token\n").unwrap();

        let prompt = epilogue_with(Some(dir.clone()), Some(dir.clone()), DEFAULT_MAX_SOUL_BYTES);
        let today = Utc::now().format("%Y-%m-%d").to_string();

        // Churniest block last: a new workspace file must not invalidate the
        // soul's or the guidance's share of the cached prefix. Anchor on the
        // `---` delimiter — the fragment names both headings in its own prose.
        let block_at = |heading: &str| prompt.find(&format!("\n---\n\n{heading}")).unwrap();
        let listing_at = block_at("# Workspace Files\n");
        assert!(block_at("# Soul\n") < listing_at, "{prompt}");
        assert!(block_at("# Project guidance (") < listing_at, "{prompt}");

        // Path relative to workspace/, UTC day, first heading as the title.
        assert!(
            prompt.contains(&format!(
                "- `notes/devbox.md` — {today} — Devbox build gotchas\n"
            )),
            "{prompt}"
        );
        // No heading: the first non-empty line stands in.
        assert!(
            prompt.contains(&format!("- `scratch.txt` — {today} — first line wins\n")),
            "{prompt}"
        );
        // A binary file is still worth knowing about, but nothing in it is
        // quotable — path and day, no title, no control bytes in the prompt.
        assert!(
            prompt.contains(&format!("- `index.bin` — {today}\n")),
            "{prompt}"
        );
        assert!(!prompt.contains("binary_token"), "{prompt}");

        // The live soul is already quoted above and superseded versions are
        // noise; hidden names are writes still in flight.
        assert!(!prompt.contains("- `soul/"), "{prompt}");
        assert!(!prompt.contains("hidden_token"), "{prompt}");
        assert!(!prompt.contains(".tmp-draft.md"), "{prompt}");
        // AGENTS.md is the launch directory's, not the workspace's.
        assert!(!prompt.contains("- `AGENTS.md`"), "{prompt}");
    }

    #[test]
    fn byte_counts_read_as_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(64 * 1024), "64 KiB");
        assert_eq!(human_bytes(132_000), "128.9 KiB");
    }

    #[test]
    fn oversized_workspace_listing_is_capped_with_a_marker() {
        let home = temp_dir("listing-cap");
        let workspace = home.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let overflow = 25;
        for i in 0..(MAX_LISTING_ENTRIES + overflow) {
            std::fs::write(workspace.join(format!("n-{i:03}.md")), "# t\n").unwrap();
        }

        let prompt = epilogue_with(
            Some(home.path().to_path_buf()),
            None,
            DEFAULT_MAX_SOUL_BYTES,
        );
        assert!(
            prompt.contains(&format!("[at least {overflow} more file(s) not listed")),
            "{prompt}"
        );
        assert!(prompt.len() < DEFAULT_AGENT_PROMPT_EPILOGUE.len() + MAX_LISTING_BYTES + 500);
    }
}
