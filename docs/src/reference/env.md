# Environment

## Read by argus

| Variable | Default | Purpose |
|---|---|---|
| `ARGUS_CONFIG` | `$XDG_CONFIG_HOME/argus/config.toml`, else `~/.config/argus/config.toml` | [Config file](config.md). |
| `ARGUS_GROUP` | none | Default group for `argus run`; overrides the config file. |
| `ARGUS_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/argus`, else `$TMPDIR/argus-$UID` | Sockets and lock. |
| `ARGUS_STATE_DIR` | `$XDG_STATE_HOME/argus`, else `~/.local/state/argus` | Registry and logs. |

Set both directory variables, and `ARGUS_CONFIG`, to run a separate, isolated argus.

## Set for every agent

| Variable | Value |
|---|---|
| `ARGUS_AGENT_ID` | The agent's ID. `argus status` and `argus wait --dir` leave this agent out; the target `self` names it. |
| `ARGUS_SOCKET` | The manager's socket. |

pi agents also get `ARGUS_PI_HOOK`, and omp agents `ARGUS_OMP_HOOK`: the
path to `argus-hook`, for argus's extension.
