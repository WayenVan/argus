# CLI

This document contains the help content for the `argus` command-line program.

## `argus`

Lightweight manager for long-running terminal agents

**Usage:** `argus <COMMAND>`

###### **Subcommands:**

* `run` — Start an agent and attach to it (use -d to leave it running in the background)
* `ps` — List agents
* `grid` — Full-screen dashboard: a live thumbnail grid of every agent's screen
* `tree` — Full-screen dashboard: a group-path tree with a live detail pane for the selected agent (same app as `argus grid`, opened on the tree mode)
* `logs` — Print an agent's recent output
* `send` — Type a prompt into an agent and press Enter, only while it is idle (or done); otherwise exit 75
* `rename` — Rename an agent: a new last segment, a full path, or `group/`
* `mv` — Move an agent into another group, keeping its last segment
* `label` — Set (key=value) or remove (key-) labels
* `ack` — Mark a finished agent as seen (done → idle)
* `events` — Print agent events as they happen
* `wait` — Block until an agent exits (exiting with its code) or reaches an activity
* `attach` — Take over an agent's terminal (detach with Ctrl-\)
* `kill` — Stop an agent (SIGTERM, then SIGKILL after 5s)
* `rm` — Remove an exited agent
* `prune` — Remove every exited agent
* `manager` — Manage the manager process



## `argus run`

Start an agent and attach to it (use -d to leave it running in the background)

**Usage:** `argus run [OPTIONS] <PROGRAM> [-- <ARGS>...]`

###### **Arguments:**

* `<PROGRAM>` — Program to run; its name selects the kind unless --kind is given
* `<ARGS>` — Arguments passed to the program

###### **Options:**

* `--name <NAME>` — Agent name (last path segment, or a full `group/name` path)
* `--in <GROUP>` — Group to create the agent in (default: $ARGUS_GROUP)
* `--cwd <CWD>` — Working directory (default: current directory)
* `-d`, `--detach` — Start in the background instead of attaching right away
* `-l`, `--label <KEY=VALUE>` — Label as key=value (repeatable)
* `--kind <KIND>` — Treat the program as this kind of agent (e.g. a wrapper script for claude)



## `argus ps`

List agents

**Usage:** `argus ps [OPTIONS] [PREFIX]`

###### **Arguments:**

* `<PREFIX>` — Only agents whose name starts with this group prefix

###### **Options:**

* `-a`, `--all` — Include exited agents
* `-l`, `--label <KEY=VALUE>` — Only agents with this label (repeatable; all must match)
* `-w`, `--watch` — Keep the list on screen and update it as agents change
* `--json`



## `argus grid`

Full-screen dashboard: a live thumbnail grid of every agent's screen

**Usage:** `argus grid [OPTIONS] [PREFIX]`

###### **Arguments:**

* `<PREFIX>` — Only agents whose name starts with this group prefix

###### **Options:**

* `-l`, `--label <KEY=VALUE>` — Only agents with this label (repeatable; all must match)



## `argus tree`

Full-screen dashboard: a group-path tree with a live detail pane for the selected agent (same app as `argus grid`, opened on the tree mode)

**Usage:** `argus tree [OPTIONS] [PREFIX]`

###### **Arguments:**

* `<PREFIX>` — Only agents whose name starts with this group prefix

###### **Options:**

* `-l`, `--label <KEY=VALUE>` — Only agents with this label (repeatable; all must match)



## `argus logs`

Print an agent's recent output

**Usage:** `argus logs [OPTIONS] <TARGET>`

###### **Arguments:**

* `<TARGET>`

###### **Options:**

* `-n`, `--bytes <BYTES>` — Only the last N bytes
* `-f`, `--follow` — Keep printing new output until the agent exits
* `--raw` — Write control sequences verbatim, including OSC 52 clipboard writes
* `--screen` — Print the manager's virtual-terminal rendering of the agent's current screen instead of the output stream (a point-in-time snapshot; requires a running agent)



## `argus send`

Type a prompt into an agent and press Enter, only while it is idle (or done); otherwise exit 75

**Usage:** `argus send [OPTIONS] <TARGET> <TEXT>`

###### **Arguments:**

* `<TARGET>`
* `<TEXT>` — The prompt; `-` reads it from stdin. Multi-line text is pasted

###### **Options:**

* `-n`, `--no-enter` — Do not press Enter after the text
* `--force` — Send even if the agent is not idle (never while it is blocked on a permission prompt)
* `-w`, `--wait` — Wait until the agent is idle instead of failing
* `--then-wait` — After sending, block until the agent finishes the turn; prints the activity it ends in (exit 1 if blocked, error or unknown)
* `--timeout <SECS>` — Give up after this many seconds, counting both waits (exit 124)



## `argus rename`

Rename an agent: a new last segment, a full path, or `group/`

**Usage:** `argus rename <TARGET> <NAME>`

###### **Arguments:**

* `<TARGET>`
* `<NAME>`



## `argus mv`

Move an agent into another group, keeping its last segment

**Usage:** `argus mv <TARGET> <GROUP>`

###### **Arguments:**

* `<TARGET>`
* `<GROUP>` — Destination group; `/` for the top level



## `argus label`

Set (key=value) or remove (key-) labels

**Usage:** `argus label <TARGET> <KEY=VALUE|KEY->...`

###### **Arguments:**

* `<TARGET>` — ID, name, or `group/**`
* `<KEY=VALUE|KEY->`



## `argus ack`

Mark a finished agent as seen (done → idle)

**Usage:** `argus ack <TARGET>`

###### **Arguments:**

* `<TARGET>`



## `argus events`

Print agent events as they happen

**Usage:** `argus events [OPTIONS]`

###### **Options:**

* `--json` — One JSON object per line



## `argus wait`

Block until an agent exits (exiting with its code) or reaches an activity

**Usage:** `argus wait [OPTIONS] <TARGET>`

###### **Arguments:**

* `<TARGET>`

###### **Options:**

* `--until <UNTIL>` — `exited`, or an activity such as `waiting`

  Default value: `exited`
* `--timeout <SECS>` — Give up after this many seconds (exit code 124)



## `argus attach`

Take over an agent's terminal (detach with Ctrl-\)

**Usage:** `argus attach [OPTIONS] <TARGET>`

###### **Arguments:**

* `<TARGET>` — ID, full group/name, or a globally unique final name

###### **Options:**

* `--ro` — Watch only: send no input and never resize the agent
* `--steal` — Disconnect every other attached terminal first
* `--replay` — Print recent output before live output
* `--allow-clipboard-replay` — Allow historical OSC 52 sequences to overwrite the clipboard



## `argus kill`

Stop an agent (SIGTERM, then SIGKILL after 5s)

**Usage:** `argus kill [OPTIONS] <TARGET>`

###### **Arguments:**

* `<TARGET>` — ID, name, or `group/**`

###### **Options:**

* `-s`, `--signal <SIGNAL>` — Signal number to send instead of SIGTERM



## `argus rm`

Remove an exited agent

**Usage:** `argus rm <TARGET>`

###### **Arguments:**

* `<TARGET>`



## `argus prune`

Remove every exited agent

**Usage:** `argus prune [OPTIONS] [PREFIX]`

###### **Arguments:**

* `<PREFIX>` — Only agents under this group prefix

###### **Options:**

* `--older-than <AGE>` — Only agents that ended longer ago than this, e.g. 30m, 24h, 7d



## `argus manager`

Manage the manager process

**Usage:** `argus manager <COMMAND>`

###### **Subcommands:**

* `start` — Start the manager if it is not running
* `restart` — Restart the manager (e.g. after upgrading); running agents keep running
* `stop` — Stop the manager; running agents keep running
* `status` — Show whether the manager is running



## `argus manager start`

Start the manager if it is not running

**Usage:** `argus manager start`



## `argus manager restart`

Restart the manager (e.g. after upgrading); running agents keep running

**Usage:** `argus manager restart`



## `argus manager stop`

Stop the manager; running agents keep running

**Usage:** `argus manager stop [OPTIONS]`

###### **Options:**

* `--kill-agents` — Also kill every running agent



## `argus manager status`

Show whether the manager is running

**Usage:** `argus manager status`



