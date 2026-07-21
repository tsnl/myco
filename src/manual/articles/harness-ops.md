# Harness ops

How to inspect, install, and repair **this** runtime (agent + host pool). Use when tools fail,
hosts look wrong, or the user asks you to update `myco` / explain the harness.

## Host PATH prerequisites

Same executables as the README **Install** section (extra binaries on `PATH`). Install on each machine
that runs the agent and/or host tools; remotes need what their host tools spawn, not only the
agent laptop.

**Required**

- **`ssh`** — attaches remotes (`ssh … myco --mode host` over NDJSON) and is used for install/diagnose over SSH.
- **`lynx`** — powers the `lynx_tui_browser` host tool (`lynx -dump` plaintext pages / search results).
- **`uv`** — hermetic Python runs (agent computer-use norm: scripts and deps without polluting the system).
- **`bash`** — host `bash` tool (one-shot `exec` and multi-turn shell sessions).
- **`bwrap`** — sandboxes `bash` `exec`/`start` by default: whole filesystem read-only except the
  working dir, `/tmp`, and toolchain caches (`~/.cargo`, `~/.rustup`, `~/.npm`, `~/.cache`); network
  unchanged. Required — sandboxed calls fail closed when missing; pass `sandbox: false` to opt out per
  call. Contains `bash` children only (the editor tool is not sandboxed).

**Recommended**

- **`git`** — worktrees/branches, repo inspection, and `git archive` when shipping local source to remotes.
- **`gh`** — GitHub CLI for PRs, issues, and release workflows the agent often drives.
- **`curl`** — `build.rs` MiniLM asset fetch at compile time, and downloading release source tarballs.

Also needed when **building from source**: stable **Rust / cargo** (and `curl` as above). Optional
`trunk` + `wasm32-unknown-unknown` only for **`crates/myco-gui`**.

## Finding configured hosts

- **Local** is always present (in-process); it is never configured.
- Remotes are the concrete `Host` aliases in **`~/.ssh/config`** (`Include`s are
  followed; wildcard `*`/`?` and negated `!` patterns are ignored; alias `local`
  is reserved). Host name == alias == SSH destination.
- **`~/.myco/config.toml`** (or `$MYCO_CONFIG` / `myco --config`) holds knobs only:
  `enable_subagent`, `attach_timeout_secs`.

- Read `~/.ssh/config` with tools when you need remote names or SSH destinations.
- Tell the user to run **`/hosts`** for live attach status (local ok/in-process; remotes idle / ok / DOWN); you cannot run slash-commands.
- Host tool field `host` must match a configured name (`local` or a remote `name`). Omitted → `local`.

## Updating / installing `myco`

**Local** uses the agent process binary / in-process worker — rebuild/reinstall the interactive
`myco` on this machine and **restart the CLI**.

For **remotes**, prefer a **same-platform binary** (release asset or build on the target)
when you are not actively developing myco; only **build from a local git tree** when you are
working on the myco codebase itself (unreleased commits, dirty worktree, or a feature branch
that must ship). Embedding weights are **compiled into** `myco`, so a correct binary is enough to
run — you do **not** deploy model files next to it. Do **not** scp/rsync a prebuilt binary
across **mismatched** OS, CPU arch, or glibc (e.g. newer glibc → older cluster fails with
`GLIBC_X.Y not found`); for those targets, compile on the machine or use a matching release.

### Choose: release snapshot vs local source tree

1. **Inspect the running agent binary** (this process):
   - `session_meta` with `action: "executable_path"` → absolute path of the agent `myco`.
   - Then via bash: `"$path" --version` (package version from clap / `CARGO_PKG_VERSION`).
2. **If you are currently working on myco** (cwd is a myco checkout, or the user asked to
   deploy local unreleased changes): build from that git tree (see **Snapshot the repo**
   below) so remotes match the working tree.
3. **Otherwise** (normal install / update): download a **source snapshot from GitHub
   Releases** for the version you want (usually the same as the local binary's
   `--version`, or a newer release the user named):

```text
https://github.com/tsnl/myco/releases
```

Typical assets / URLs (GitHub):

```bash
# Prefer the release tag that matches (or is newer than) local `myco --version`.
# Source tarball for tag v0.1.0 (example):
VER=0.1.0
curl -fsSL -o /tmp/myco-src.tgz \
  "https://github.com/tsnl/myco/archive/refs/tags/v${VER}.tar.gz"
# Or the auto "Source code (tar.gz)" asset on the release page for that tag.
```

Unpack on each remote and `cargo install` there (next section). If no suitable release
exists yet, fall back to archiving the local git tree.

### Snapshot the repo (local git tree only)

Use this when deploying **work-in-progress** or unreleased commits from a myco checkout.

From the myco git root, create a source snapshot with **`git archive`** (tree only — tracked
files at a commit/tree; not untracked files, and not uncommitted dirty work unless you archive a
commit that includes them):

```bash
# Clean committed snapshot of HEAD (usual case):
git archive --format=tar.gz -o /tmp/myco-src.tgz HEAD

# Include current dirty tracked changes: temporary tree object, then archive it:
REF=$(git stash create)   # empty if worktree is clean — fall back to HEAD
git archive --format=tar.gz -o /tmp/myco-src.tgz "${REF:-HEAD}"
```

`git archive` is the right “tarball of this repo state” tool. Commit (or use `git stash create`)
first if the install must include uncommitted edits. Untracked files are never in the archive;
add/commit them if they are required to build.

### Embedding weights (MiniLM / Candle) — fully embedded in the binary

Semantic search bakes all-MiniLM-L6-v2 (Candle) into `myco` at **compile time**
(`build.rs` stages under `OUT_DIR` + `include_bytes!`). Runtime never reads weight
files and never downloads from Hugging Face.

| Stage | What you need |
|-------|----------------|
| **Running / shipping** | The **`myco` binary only** for that OS/CPU/libc. No model pack, no `embed_weights/` on the target. |
| **Compiling from source** | Network so `build.rs` (`hf-hub`) can populate the shared Hub cache and stage into `OUT_DIR` (or seed via warm `HF_HOME` / `MYCO_EMBED_CACHE` / `src/text_search/embed_weights/`). |

Source tarballs / crates.io packages / `git archive` do not ship the ~87 MiB weight
files (gitignored; not part of the package). That only matters when **building**; a
finished binary already contains the weights. Details:
`src/text_search/embed_weights/README.md`.

Because weights are embedded, a **release only needs platform-matched binaries**. Do not scp
binaries across mismatched OS/arch/glibc; for those hosts, build there or use a matching
release asset.

### Install on each remote host (build from source)

```bash
HOST=devbox   # Host alias from ~/.ssh/config (== myco host name)
# /tmp/myco-src.tgz is either a GitHub release source tarball or a local git archive.
scp -o BatchMode=yes /tmp/myco-src.tgz "$HOST:/tmp/myco-src.tgz"
ssh -o BatchMode=yes "$HOST" 'set -euo pipefail
  rm -rf ~/src/myco-src ~/src/myco-src-extract
  mkdir -p ~/src/myco-src-extract ~/.local/bin
  tar -xzf /tmp/myco-src.tgz -C ~/src/myco-src-extract
  rm -f /tmp/myco-src.tgz
  # GitHub release tarballs nest under myco-<tag>/; `git archive` is usually flat.
  entries=(~/src/myco-src-extract/*)
  if [ ${#entries[@]} -eq 1 ] && [ -d "${entries[0]}" ]; then
    mv "${entries[0]}" ~/src/myco-src
    rmdir ~/src/myco-src-extract 2>/dev/null || rm -rf ~/src/myco-src-extract
  else
    mv ~/src/myco-src-extract ~/src/myco-src
  fi
  export PATH="$HOME/.cargo/bin:$PATH"
  command -v cargo >/dev/null || { echo "cargo/rustc required on host"; exit 1; }
  # build.rs uses hf-hub (shared HF cache) then stages MiniLM assets into OUT_DIR.
  cargo install --path ~/src/myco-src --force --locked --root "$HOME/.local"
  # ~/.local/bin is the usual remote install path in multi-host setups
  ~/.local/bin/myco --version
'
```

- Require **Rust/cargo** (and network once for `hf-hub` MiniLM assets, or a warm Hub cache) when building from source. Prefer a
  prebuilt **same-platform** binary when available (weights already inside).
- Remotes need `myco` on the **remote** PATH used by non-interactive SSH (`BatchMode`);
  `~/.local/bin` or `~/.cargo/bin` are common — verify with
  `ssh -o BatchMode=yes <alias> 'command -v myco; myco --version'`.
- After replacing binaries, the **interactive CLI must be restarted** to load a new agent binary;
  remote **host** workers respawn on next tool use (or after `/hosts` shows DOWN and reconnect).
- Ask before destructive remote installs; prefer installing into user prefixes (`~/.local`,
  `~/.cargo`) over system paths.

## Diagnosis checklist

When tools fail or the user asks why something is broken, investigate with tools:

1. **Host down / unavailable**
   - **Local** never needs a host subprocess; if local tools fail, debug the agent process itself.
   - Read `~/.ssh/config` for `Host` aliases (remote names == destinations);
     `~/.myco/config.toml` (or `$MYCO_CONFIG`) only for knobs.
   - On remote: `ssh -o BatchMode=yes <alias> 'which myco; myco --help'` via the
     **local** host's bash. If missing/outdated: install a **binary built for that
     platform** (release asset — weights already embedded), or **build on that host**
     from source. Do not copy binaries across mismatched OS/arch/glibc.
   - Confirm SSH alias works: `ssh -o BatchMode=yes <alias> true`.
   - Startup checks expected executables on the **agent** machine (`bash`,
     `lynx`; `ssh`/`ssh-add`/`ssh-keygen` when remotes are configured) and
     reports missing ones in the startup WARNING block — the user must install
     them and restart myco. Remote hosts report missing programs as tool
     errors at call time.
   - If auth fails with BatchMode: check `ssh-add -l` and the startup ssh-agent
     preflight (silent when clean; problems open a WARNING block before the first
     USER block).
     Unlock with `ssh-add` / `ssh-add --apple-use-keychain <key>` (myco cannot prompt on the
     NDJSON pipe). Restart myco after loading keys.
   - Suggest user run `/hosts` after fixes (requires CLI restart to re-attach remotes).

2. **Wrong machine / wrong files**
   - Check whether `host` was set; default is always `local`.
   - `bash` `uname -n` / `pwd` / `hostname` on the intended `host`.

3. **Session / state confusion**
   - Conversation resume ≠ restored bash sessions or editor state.
   - Bash sessions die when the host process exits (CLI exit, host crash, SSH drop). Local
     in-process sessions die with the agent process.

4. **Explain product limits honestly**
   - No heartbeat in V1: remote liveness is next tool error; local is always in-process.
   - No mid-flight cancel over the host pipe yet; Ctrl-C cancels the agent turn locally.
   - You cannot invoke slash-commands; tell the user which to run.

When helping the user change config, prefer **surgical edits** to `~/.ssh/config` (hosts) or
`~/.myco/config.toml` (knobs) and show a minimal diff. Ask before destructive remote installs.
