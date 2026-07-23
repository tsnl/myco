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
tools (`session_meta`) stay in the agent process; host tools (`bash`, editor, `manual`, text
search, `lynx_tui_browser`) run on a host worker (local in-process or remote).

**Browse runtime docs with the `manual` tool** (`list` / `get` by id) or `myco --help <id>`.
Article ids: `overview`, `cli`, `harness-ops`.

Quick map (details in `manual`):
- Hosts: every concrete `Host` alias in `~/.ssh/config` is a remote host (`Include`s followed);
  local is always on. `~/.myco/config.toml` (or `$MYCO_CONFIG`) holds knobs only
  (`attach_timeout_secs`).
- Sessions: `~/.myco/session/{shard}/{id}.json` — use `session_meta`, not raw file edits.
- Host tools take optional `host`; omitted → **`local`** (in-process). Remotes are lazy on first use.
- `bash`: prefer optional `cwd` on `exec`/`start` over `cd … &&` (leading `cd` in `command` is rejected).
- Text search: host **persistently indexes** (watched) `.claude/skills`, `SKILL.md` folders, and
  `AGENTS.md`/`CLAUDE.md`. Use `indexed_exact_text_search` / `indexed_semantic_text_search` for
  skills & guidance; `index_directory` registers more small scopes (stays watched until drop).
  **Prefer `bash` + `rg`/`grep` for large code trees** — do not index monorepos or `node_modules`.
- You cannot run slash-commands (`/hosts`, `/session`, …); tell the user which to run.
- Updating `myco` on **remote** hosts: compile **on the target** (see `manual` `harness-ops`).
  If developing myco, archive the local git tree; else download a source snapshot from
  https://github.com/tsnl/myco/releases (match `session_meta` `executable_path` +
  `myco --version`). **Separately** ensure MiniLM embed weights are available for the
  build (`build.rs` uses `hf-hub` into the shared Hugging Face cache, then
  stages into `OUT_DIR` — weights are **not** in git archive / crates.io /
  GitHub source tarballs). Never scp prebuilt
  binaries across machines (glibc/arch mismatch).

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
one prompt per line — each line submits a turn — and `read` until the next `USER n/m` header, which
marks the turn boundary (colors and wrapping switch off automatically when piped). Ask for terse
summaries; `close` the session when done. The child's session is hidden (`kind: subagent`,
parented to yours) in the shared `~/.myco/session/` store — read it later via `session_meta`
get-by-id or `list` with `include_hidden: true`.

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

/// Backstop so one runaway soul revision cannot bloat every future prompt
/// (the fragment asks for about a screenful; same cap as the session
/// scratchpad). The truncation marker tells the agent to write a shorter one.
const MAX_SOUL_BYTES: usize = 64 * 1024;

/// The epilogue plus the current soul (`~/.myco/workspace/soul/`, respecting
/// `MYCO_HOME`), when present. Read at model build time — session start, model
/// switch, each worker spawn — so a running agent's prompt never changes
/// mid-conversation and the cached conversation prefix stays valid.
pub fn agent_prompt_epilogue() -> String {
    epilogue_with_home(crate::session::myco_home().ok())
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

/// [`agent_prompt_epilogue`] against an explicit myco home, so tests need no
/// process-global `MYCO_HOME` override.
fn epilogue_with_home(home: Option<std::path::PathBuf>) -> String {
    let soul = home.and_then(|home| latest_soul(&home.join("workspace").join("soul")));
    match soul {
        Some((name, mut soul)) => {
            if soul.len() > MAX_SOUL_BYTES {
                let mut end = MAX_SOUL_BYTES;
                while !soul.is_char_boundary(end) {
                    end -= 1;
                }
                soul.truncate(end);
                soul.push_str("\n\n[soul truncated at 64 KiB — write a shorter revision]");
            }
            format!(
                "{DEFAULT_AGENT_PROMPT_EPILOGUE}\n---\n\n# Soul\n\n\
                 (current version: soul/{name})\n\n{soul}\n"
            )
        }
        None => DEFAULT_AGENT_PROMPT_EPILOGUE.to_string(),
    }
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
        // runtime catalog pointer, not full policy-as-articles
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("`harness-ops`"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("indexed_exact_text_search"));
        // Free-form workspace policy: maildir-style soul versions, the
        // recall/record habit, and the consistency caution.
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("Workspace & soul"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("~/.myco/workspace/soul/"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("write-once, never edited in place"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("consult and maintain them often"));
        assert!(DEFAULT_AGENT_PROMPT_EPILOGUE.contains("weakly consistent"));
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
        let epilogue = || epilogue_with_home(Some(dir.clone()));

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
            "x".repeat(MAX_SOUL_BYTES * 2),
        )
        .unwrap();
        let prompt = epilogue();
        assert!(prompt.contains("[soul truncated at 64 KiB"), "{prompt}");
        assert!(prompt.len() < DEFAULT_AGENT_PROMPT_EPILOGUE.len() + MAX_SOUL_BYTES + 200);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
