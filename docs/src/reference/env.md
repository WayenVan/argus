# Environment

## Read by argus

| Variable | Default | Purpose |
|---|---|---|
| `ARGUS_GROUP` | none | Default group for `argus run`. |
| `ARGUS_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/argus`, else `$TMPDIR/argus-$UID` | Sockets and lock. |
| `ARGUS_STATE_DIR` | `$XDG_STATE_HOME/argus`, else `~/.local/state/argus` | Registry and logs. |

Set both directory variables to run a separate, isolated argus.

## Set for every agent

| Variable | Value |
|---|---|
| `ARGUS_AGENT_ID` | The agent's ID. |
| `ARGUS_SOCKET` | The manager's socket. |

pi agents also get `ARGUS_PI_HOOK`, and omp agents `ARGUS_OMP_HOOK`: the
path to `argus-hook`, for argus's extension.
