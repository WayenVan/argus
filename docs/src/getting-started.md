# Getting started

## Install

argus is three binaries, and all of them must be installed:

```sh
cargo install --path crates/argus
cargo install --path crates/argus-holder
cargo install --path crates/argus-hook
```

After upgrading, restart the manager so it runs the new code. Running agents
are not affected:

```sh
argus manager restart
```

## First agent

```sh
argus run claude          # start Claude Code as `claude-1` and attach
```

Detach with `Ctrl-\`. The agent keeps running.

```sh
argus ps                  # list agents
argus attach claude-1     # attach again
argus grid                # dashboard
```

The manager starts on its own the first time you run a command.
