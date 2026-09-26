# CLI

This document contains the help content for the `argus` command-line program.

## `argus`

Lightweight manager for long-running terminal agents

**Usage:** `argus <COMMAND>`

###### **Subcommands:**

* `run` — Start an agent and attach to it (use -d to leave it running in the background)
* `ps` — List agents
* `inspect` — Show one agent in full
* `status` — Agents working in a directory, and whether they are all free. Leaves out the agent running this command
* `grid` — Full-screen dashboard: a live thumbnail grid of every agent's screen
* `tree` — Full-screen dashboard: a group-path tree with a live detail pane for the selected agent (same app as `argus grid`, opened on the tree mode)
* `logs` — Print an agent's recent output
* `send` — Type a prompt into an agent and press Enter, only while it is idle (or done); otherwise exit 75
* `rename` — Rename an agent: a new last segment, a full path, or `group/`
* `mv` — Move an agent into another group, keeping its last segment
* `label` — Set (key=value) or remove (key-) labels
* `ack` — Mark a finished agent as seen (done → idle)
* `events` — Print agent events as they happen
* `wait` — Block until agents are free (their turn is over) or reach another state; with several, until all have. One agent waited on to exit passes on its exit code
* `attach` — Take over an agent's terminal (detach with Ctrl-\)
* `kill` — Stop an agent (SIGTERM, then SIGKILL after 5s)
* `rm` — Remove an exited agent
* `prune` — Remove every exited agent
* `manager` — Manage the manager process
* `setup` — One-time changes to an agent's own configuration that widen what it may do; shows them and asks first. Without an agent, sets up every agent found on PATH



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
* `--json` — Print JSON (one object per line) instead of text



## `argus ps`

List agents

**Usage:** `argus ps [OPTIONS] [PREFIX]`

###### **Arguments:**

* `<PREFIX>` — Only agents whose name starts with this group prefix

###### **Options:**

* `-a`, `--all` — Include exited agents
* `-l`, `--label <KEY=VALUE>` — Only agents with this label (repeatable; all must match)
* `-w`, `--watch` — Keep the list on screen and update it as agents change
* `--json` — Print JSON (one object per line) instead of text



## `argus inspect`

Show one agent in full

**Usage:** `argus inspect [OPTIONS] <TARGET>`

###### **Arguments:**

* `<TARGET>` — ID, name, or `self` for the agent running this command

###### **Options:**

* `--screen` — Also print its current screen as plain text
* `--last <N>` — Also print its last N finished turns (default 1): each prompt and final reply, as its hooks reported them
* `--json` — Print JSON (one object per line) instead of text



## `argus status`

Agents working in a directory, and whether they are all free. Leaves out the agent running this command

**Usage:** `argus status [OPTIONS] [PATH]`

###### **Arguments:**

* `<PATH>` — Directory (default: the current one)

###### **Options:**

* `--scope <SCOPE>` — Which working directories count as in PATH

  Default value: `under`

  Possible values:
  - `under`:
    Working directory is PATH or below it
  - `exact`:
    Working directory is exactly PATH
  - `repo`:
    Working directory is in the same git repository as PATH, including its other worktrees

* `-a`, `--all` — Include exited agents
* `--include-self` — Include the agent running this command
* `-l`, `--label <KEY=VALUE>` — Only agents with this label (repeatable; all must match)
* `--json` — Print JSON (one object per line) instead of text



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
* `--then-wait` — After sending, block until the agent finishes the turn this prompt started (like `wait --after`); prints the activity it ends in. Keeps waiting while it is blocked; fails with code `stuck` if the turn ends in an error or the agent goes unknown
* `--timeout <SECS>` — Give up after this many seconds, counting both waits (exit 124)
* `--json` — Print JSON (one object per line) instead of text



## `argus rename`

Rename an agent: a new last segment, a full path, or `group/`

**Usage:** `argus rename [OPTIONS] <TARGET> <NAME>`

###### **Arguments:**

* `<TARGET>`
* `<NAME>`

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus mv`

Move an agent into another group, keeping its last segment

**Usage:** `argus mv [OPTIONS] <TARGET> <GROUP>`

###### **Arguments:**

* `<TARGET>`
* `<GROUP>` — Destination group; `/` for the top level

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus label`

Set (key=value) or remove (key-) labels

**Usage:** `argus label [OPTIONS] <TARGET> <KEY=VALUE|KEY->...`

###### **Arguments:**

* `<TARGET>` — ID, name, or `group/**`
* `<KEY=VALUE|KEY->`

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus ack`

Mark a finished agent as seen (done → idle)

**Usage:** `argus ack [OPTIONS] <TARGET>`

###### **Arguments:**

* `<TARGET>`

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus events`

Print agent events as they happen

**Usage:** `argus events [OPTIONS]`

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus wait`

Block until agents are free (their turn is over) or reach another state; with several, until all have. One agent waited on to exit passes on its exit code

**Usage:** `argus wait [OPTIONS] [TARGETS]...`

###### **Arguments:**

* `<TARGETS>` — IDs, names or `group/**`

###### **Options:**

* `--dir <PATH>` — Wait for every running agent working in this directory instead, as `argus status` lists them (not the one running this command)
* `--scope <SCOPE>` — With --dir: which working directories count as in PATH [default: under]

  Possible values:
  - `under`:
    Working directory is PATH or below it
  - `exact`:
    Working directory is exactly PATH
  - `repo`:
    Working directory is in the same git repository as PATH, including its other worktrees

* `-l`, `--label <KEY=VALUE>` — With --dir: only agents with this label (repeatable)
* `--until <AVAILABILITY>` — The availability to wait for: `free`, `active`, `attention`, `unknown`, or `exited` (the process ended)

  Default value: `free`
* `--until-activity <ACTIVITY>` — Wait for one exact activity instead, such as `done` or `tool:Bash`
* `--after <N>` — Only once the agent has finished a turn after turn N (its `turns` is past N), e.g. the `turn` `send --json` printed. One agent; keeps waiting while it is blocked; fails with code `stuck` if the turn ends in an error or the agent goes unknown first
* `--timeout <SECS>` — Give up after this many seconds (exit code 124)
* `--json` — Print JSON (one object per line) instead of text



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
* `--json` — Print JSON (one object per line) instead of text



## `argus rm`

Remove an exited agent

**Usage:** `argus rm [OPTIONS] <TARGET>`

###### **Arguments:**

* `<TARGET>`

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus prune`

Remove every exited agent

**Usage:** `argus prune [OPTIONS] [PREFIX]`

###### **Arguments:**

* `<PREFIX>` — Only agents under this group prefix

###### **Options:**

* `--older-than <AGE>` — Only agents that ended longer ago than this, e.g. 30m, 24h, 7d
* `--json` — Print JSON (one object per line) instead of text



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

**Usage:** `argus manager start [OPTIONS]`

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus manager restart`

Restart the manager (e.g. after upgrading); running agents keep running

**Usage:** `argus manager restart [OPTIONS]`

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus manager stop`

Stop the manager; running agents keep running

**Usage:** `argus manager stop [OPTIONS]`

###### **Options:**

* `--kill-agents` — Also kill every running agent
* `--json` — Print JSON (one object per line) instead of text



## `argus manager status`

Show whether the manager is running

**Usage:** `argus manager status [OPTIONS]`

###### **Options:**

* `--json` — Print JSON (one object per line) instead of text



## `argus setup`

One-time changes to an agent's own configuration that widen what it may do; shows them and asks first. Without an agent, sets up every agent found on PATH

**Usage:** `argus setup [OPTIONS]
       setup <COMMAND>`

###### **Subcommands:**

* `codex` — Let Codex agents run `argus label self` and `argus ps`/`status`/`inspect`/`wait` outside the sandbox, via rules in $CODEX_HOME/rules/argus.rules

###### **Options:**

* `-y`, `--yes` — Make every change without asking
* `--remove` — Undo every change instead
* `--json` — Print JSON (one object per line) instead of text



## `argus setup codex`

Let Codex agents run `argus label self` and `argus ps`/`status`/`inspect`/`wait` outside the sandbox, via rules in $CODEX_HOME/rules/argus.rules

**Usage:** `argus setup codex [OPTIONS]`

###### **Options:**

* `-y`, `--yes` — Write the rules without asking
* `--remove` — Delete argus's rules file instead
* `--json` — Print JSON (one object per line) instead of text



