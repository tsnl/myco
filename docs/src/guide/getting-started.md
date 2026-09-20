# Getting started

Myco runs the conversation and model requests on your computer. Tools run on
the always-available `local` host or on a remote host you name in SSH config.
You can start with only local tools.

## Install

With stable Rust and Cargo installed:

```bash
cargo install myco --locked
myco --version
```

Have `bash` and `uv` available for shell work and Python tooling. Install `tmux`
and `fzf` for the session picker, and OpenSSH for remote hosts. `git`, `gh`,
`rg`, and `curl` are useful programs for the agent to use through bash. Startup
reports missing expected executables; `/resume <id>` works without the picker.

This book tracks `main`. To run the same source from a checkout:

```bash
git clone https://github.com/tsnl/myco.git
cd myco
cargo install --path . --locked
```

## Configure one model

Myco has no built-in model catalog. Create the selected profile's config
directory (the default profile is shown here):

```bash
mkdir -p ~/.myco/profiles/default
```

Save this as `~/.myco/profiles/default/config.toml`. Replace the endpoint,
`api_id`, and context window with values supported by your model server:

```toml
model = "local"

[gateways.local]
protocol = "openai-completions"
base_url = "http://localhost:11434/v1"

[models.local]
gateway = "local"
api_id = "YOUR_SERVED_MODEL_ID"
thinking = "none"
context_window = 32768
```

This example expects an already-running Chat Completions compatible server;
Myco does not install or start that server. For a hosted gateway, set its base
URL and add an authentication source, for example:

```toml
# Place inside the gateway table.
auth = { source = "env", var_name = "MY_MODEL_API_KEY" }
```

Use the protocol your endpoint implements. The
[configuration guide](configuration.md) explains all three protocols and
credential sources; the [manual](../manual/overview.md#models--credentials-the-catalog)
contains fuller catalog examples.

## Start in your project

```bash
cd /path/to/your/project
myco
```

Ask for a bounded first task, such as “Explain the entry points in this
repository.” Myco reads project guidance from `AGENTS.md` or `CLAUDE.md` at
startup. Use `/hosts` to inspect available execution hosts, `/session` to see
the current session, and `/help` for controls.

Enter submits a message. **Alt-Enter** or **Ctrl-J** inserts a newline.
**Ctrl-C** cancels the current turn; **Ctrl-D** or `/exit` saves and quits.
Next, read [everyday use](everyday-use.md) and [sessions](sessions.md).
