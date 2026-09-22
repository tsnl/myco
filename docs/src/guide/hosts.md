# Remote hosts

Myco keeps model calls, credentials, and the conversation on the local host.
A remote host runs tool services over SSH. The `local` worker is always
in-process and requires no separate worker command.

## Add an SSH alias

Remote hosts come from concrete aliases in `~/.ssh/config`, including files
referenced by `Include`. Put connection details there:

```sshconfig
Host devbox
    HostName devbox.example.com
    User developer
    IdentityFile ~/.ssh/id_ed25519
```

Wildcard and negated aliases do not become Myco hosts. The name `local` is
reserved. Myco uses non-interactive SSH with `BatchMode=yes`, so arrange
authentication before starting it.

```bash
ssh -o BatchMode=yes devbox true
ssh -o BatchMode=yes devbox 'command -v myco; myco --version'
```

Install the **same Myco package version** on both hosts. Build on the remote
host or use a binary matching its OS, CPU, and libc. An interactive login may
have a different PATH from the non-interactive SSH command; verify the latter.
The remote needs the programs its tools execute, including bash. It does not
need your model catalog or API credentials.

## Direct work to a host

Start Myco and issue a tool call on the named host. An idle remote is normal: it attaches on its
first tool call. Ask, for example, “On `devbox`, inspect the build logs under
`/srv/project`.” Host tools accept a `host` field; omitting it selects `local`.

Bash working directories come from the host process. To run somewhere else,
the agent uses a command such as `cd /srv/project && cargo test`; the bash
tool has no separate working-directory field. Persistent shell state requires
a started bash session. Session IDs are specific to both the host and their
owning session runtime.

## Operate and diagnose

The tool block reports connection failures. A remote attach failure
is reported as a tool error; it does not make the local host unavailable.
Remote workers connect lazily with `ssh … myco --mode host` and exchange
newline-delimited JSON. Keep non-interactive startup output from interfering
with that protocol.

If many nested local agents use the same remote, OpenSSH connection sharing
can reuse authentication and transport. Configuration and installation recipes
are in the [harness operations manual](../manual/harness-ops.md).

After a remote process exits or SSH disconnects, its live tools are gone.
Saved conversation history remains an account of earlier observations;
reconnecting is not a shell-state restore.
