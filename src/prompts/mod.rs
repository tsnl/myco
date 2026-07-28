//! Shared system-prompt fragments for myco agents.
//!
//! Always-on agent policy (worktrees, computer-use, coding norms, user
//! authority, the agent workspace) lives here. Longer runtime docs live in
//! [`crate::manual`] and are browsed via the `manual` host tool /
//! `myco --help [id]`.

/// Epilogue appended to every agent system prompt.
pub const DEFAULT_AGENT_PROMPT_EPILOGUE: &str = concat!(
    r#"
When generating a response, please follow these guidelines.

Note that this section onward (the Myco Agent Prompt Epilogue) is made available to every myco
agent in the system prompt, including nested ones.

---

# Myco Runtime Manual

You are running inside **myco**: a mycelial agent runtime. The same agent pattern repeats at every
scale — supervisors drive **nested myco agents** as ordinary commands (see Nested Agents below),
and tools run on **hosts** (hands) spanning local and remote machines. The **local** host is always
enabled **in-process** (no subprocess). Remotes use `ssh … myco --mode host` over NDJSON. Local
tools (`session_meta`) stay in the agent process; host tools (`bash`, editor, `manual`) run on
a host worker (local in-process or remote).

**Browse runtime docs with the `manual` tool** (`list` / `get` by id) or `myco --help <id>`.
Article ids: `overview`, `cli`, `harness-ops`.

Quick map (details in `manual`):
- Hosts: every concrete `Host` alias in `~/.ssh/config` is a remote host (`Include`s followed);
  local is always on. `~/.myco/config.toml` (or `$MYCO_CONFIG`) holds knobs only
  (`attach_timeout_secs`, `max_soul_bytes`).
- Sessions: `~/.myco/session/{shard}/{id}.json` — use `session_meta`, not raw file edits.
- Host tools take optional `host`; omitted → **`local`** (in-process). Remotes are lazy on first use.
- **To act on a remote machine, set `host` on the tool call — do not run `ssh <alias> …` from
  local `bash`.** A host worker keeps one persistent SSH connection, so each call skips connection
  setup, and you get real bash sessions, the editor, and search on that machine. Shell out to
  `ssh`/`scp` only for what a host worker cannot do itself: diagnosing a DOWN host, installing
  `myco` there, or moving files between machines.
- `bash`: prefer optional `cwd` on `exec`/`start` over `cd … &&` (leading `cd` in `command` is rejected).
- Text search: use `bash` + `rg`/`grep` (`rg` for code trees; `grep -r` as fallback). Project
  guidance lives in `AGENTS.md`/`CLAUDE.md` and skill packs (`.claude/skills`, `SKILL.md`
  folders) — read them with the editor or `rg` when the task touches how this project works.
- You cannot run slash-commands (`/hosts`, `/session`, …); tell the user which to run.
- Updating `myco` on **remote** hosts: compile **on the target** (see `manual` `harness-ops`).
  If developing myco, archive the local git tree; else download a source snapshot from
  https://github.com/tsnl/myco/releases (match `session_meta` `executable_path` +
  `myco --version`). Never scp prebuilt binaries across machines (glibc/arch mismatch).

---

# Nested Agents

Context is precious. For ephemeral, task-specific context — and for complex, multi-step tasks —
delegate to a nested agent: `myco` drives itself as an ordinary interactive command.

Nest **on the local host only**. The brain stays on this machine — model access, config, keys, and
the session store are shared by construction — and a nested agent reaches remote machines through
its own host pool exactly as you do. Remote hosts stay hands, not brains: they need only `myco` on
PATH plus SSH, never config or keys. (Many myco processes sharing the same remotes multiplex
cleanly over one SSH connection per host with ControlMaster — see `manual` `harness-ops`.)

Recipe: find your own session id (`session_meta` action=get), then `bash` action=start with
`command: "myco --parent-session <your-session-id>"` (add `--model <key>` to pick a model). `write`
one prompt per line, ending each `write` with `"\n"` — each line is a self-contained turn kept to
a single line, and only the trailing newline submits it. Stdin passes through unmodified, so a
first turn written without `"\n"` is the classic hang: the child never sees a complete line while
you wait on `read`. `read` until the next `USER n/m` header, which marks the turn boundary (colors
and wrapping switch off automatically when piped). Ask for terse
summaries; `close` the session when done. The child's session is hidden (`kind: subagent`,
parented to yours) in the shared `~/.myco/session/` store — read it later via `session_meta`
get-by-id or `list` with `include_hidden: true`.

One-shot delegation: for a single self-contained task, skip the live session — a plain one-shot
`bash` run of `myco -p "<task>" --parent-session <your-session-id>` does one turn and exits.
Stdout is the answer text alone (no headers to parse; process exit is the turn boundary) and
stderr ends with `session=<id>` for later reads. `-p` composes with `--fork` and `--model`.
Prefer a live session for real back-and-forth: `myco -p "…" --resume <child-id>` can append
follow-up turns, but each pays full process startup. Inside a **live** bash session, append
`</dev/null` — with a prompt argument `-p` still drains piped stdin as context, and a live
session's open stdin never EOFs (one-shot `bash` runs are safe; their stdin is null).

Context forking: add `--fork` to seed the child with your session's saved conversation instead of
a blank context. Fork when the task needs what you already know (decisions so far, investigation,
the user's intent); start blank when the task is self-contained — a fork begins at your context
size and has less headroom. Launch forks on your own model (`--model` with the catalog key stamped
at the end of this prompt): a same-model fork's first request re-reads your cached prompt prefix at
a fraction of full input cost, while a different model is legal but starts cold (pass `--effort`
too if yours was changed from the default). Your session file is checkpointed mid-turn after each
user message and completed tool round, so a fork sees the current user request and finished tool
rounds — never tool calls still in flight, its own launch included; put anything newer in the first
prompt line you write to it.

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

/// The epilogue plus the current soul (`~/.myco/workspace/soul/`, respecting
/// `MYCO_HOME`, capped at `max_soul_bytes`) and the launch directory's project
/// guidance (`AGENTS.md` / `CLAUDE.md`), when present. Read at model build
/// time — session start, model switch, each worker spawn — so a running
/// agent's prompt never changes mid-conversation and the cached conversation
/// prefix stays valid.
pub fn agent_prompt_epilogue(max_soul_bytes: usize) -> String {
    epilogue_with(
        crate::session::myco_home().ok(),
        std::env::current_dir().ok(),
        max_soul_bytes,
    )
}

/// Whether the soul the next agent prompt will carry is cut short by the
/// `max_soul_bytes` cap. Startup calls this to warn before the first turn;
/// `None` means the soul fits (or there is none).
pub fn soul_truncation(max_soul_bytes: usize) -> Option<SoulTruncation> {
    let home = crate::session::myco_home().ok()?;
    let dir = home.join("workspace").join("soul");
    capped_soul(&dir, max_soul_bytes)?.2
}

/// The current soul snapshot: filename and trimmed contents of the
/// lexicographically last visible `*.md` in `workspace/soul/`. Versions are
/// write-once maildir-style files, so "newest name wins" is the whole
/// contract — a whitespace-only newest version reads as "no soul".
fn latest_soul(dir: &std::path::Path) -> Option<(String, String)> {
    let mut versions: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(dir)
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
fn epilogue_with(
    home: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    max_soul_bytes: usize,
) -> String {
    let mut prompt = DEFAULT_AGENT_PROMPT_EPILOGUE.to_string();
    let soul =
        home.and_then(|home| capped_soul(&home.join("workspace").join("soul"), max_soul_bytes));
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
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epilogue_includes_always_on_policy() {
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("Git worktrees for new features"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("Computer use"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("Think Before Coding"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("User authority & privileged operations"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("force-merge"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("manual"));
        // Nested-agent recipe: prompts are single-line, self-contained turns,
        // submitted only by a trailing newline.
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("self-contained turn"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains(r#"ending each `write` with `"\n"`"#));
        // Remote work goes through the `host` field, not local `ssh`.
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("do not run `ssh <alias> …` from"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("persistent SSH connection"));
        // runtime catalog pointer, not full policy-as-articles
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("`harness-ops`"));
        // Search guidance is bash-first; myco ships no search tools of its own.
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("`rg`"));
        assert!(!DEFAULT_AGENT_PROMPT_EPILOGUE.contains("indexed_exact_text_search"));
        // Free-form workspace policy: maildir-style soul versions, the
        // recall/record habit, and the consistency caution.
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("Workspace & soul"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("~/.myco/workspace/soul/"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("write-once, never edited in place"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("consult and maintain them often"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("weakly consistent"));
    }

    /// Project guidance from the launch directory is appended at model build
    /// time — AGENTS.md preferred, CLAUDE.md fallback, absent = no section.
    #[test]
    fn project_guidance_is_appended_from_cwd() {
        let dir = std::env::temp_dir().join(format!("myco-guidance-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
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

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_recipe_and_model_stamp_are_documented() {
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("Context forking"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("--fork"));
        // The epilogue points at the stamp; the stamp names the key and flag.
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("at the end of this prompt"));
        let stamp = model_stamp("grok-4");
        assert!(stamp.contains("# Current Model"), "{stamp}");
        assert!(stamp.contains("`grok-4`"), "{stamp}");
        assert!(stamp.contains("--model grok-4"), "{stamp}");
    }

    #[test]
    fn newest_soul_version_is_appended_to_the_epilogue() {
        let dir = std::env::temp_dir().join(format!("myco-soul-{}", uuid::Uuid::new_v4()));
        let soul_dir = dir.join("workspace").join("soul");
        std::fs::create_dir_all(&soul_dir).unwrap();
        let epilogue = || epilogue_with(Some(dir.clone()), None, DEFAULT_MAX_SOUL_BYTES);

        // No versions: the epilogue alone.
        assert_eq!(epilogue(), DEFAULT_AGENT_PROMPT_EPILOGUE);

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
        assert_eq!(epilogue(), DEFAULT_AGENT_PROMPT_EPILOGUE);

        // An oversized version is truncated with a visible marker, keeping
        // the prompt bounded no matter what got written.
        std::fs::write(
            soul_dir.join("20290101T0000-dddd.md"),
            "x".repeat(DEFAULT_MAX_SOUL_BYTES * 2),
        )
        .unwrap();
        let prompt = epilogue();
        assert!(prompt.contains("[soul truncated at 64 KiB"), "{prompt}");
        assert!(prompt.len() < DEFAULT_AGENT_PROMPT_EPILOGUE.len() + DEFAULT_MAX_SOUL_BYTES + 200);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cap_is_configurable_and_reports_what_it_cut() {
        let dir = std::env::temp_dir().join(format!("myco-soul-cap-{}", uuid::Uuid::new_v4()));
        let soul_dir = dir.join("workspace").join("soul");
        std::fs::create_dir_all(&soul_dir).unwrap();
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

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lowering_the_soul_cap_leaves_project_guidance_alone() {
        // `max_soul_bytes` is named for the soul; shrinking it to shorten a
        // soul must not silently start cutting AGENTS.md too.
        let dir = std::env::temp_dir().join(format!("myco-both-{}", uuid::Uuid::new_v4()));
        let soul_dir = dir.join("workspace").join("soul");
        std::fs::create_dir_all(&soul_dir).unwrap();
        std::fs::write(soul_dir.join("20260101T0000-aaaa.md"), "s".repeat(4000)).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "g".repeat(4000)).unwrap();

        let prompt = epilogue_with(Some(dir.clone()), Some(dir.clone()), 1024);
        assert!(
            prompt.contains("[soul truncated at 1 KiB of 3.9 KiB"),
            "{prompt}"
        );
        assert!(!prompt.contains("[project guidance truncated"), "{prompt}");
        assert!(prompt.contains(&"g".repeat(4000)), "{prompt}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn byte_counts_read_as_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(64 * 1024), "64 KiB");
        assert_eq!(human_bytes(132_000), "128.9 KiB");
    }
}
