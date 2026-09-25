# Scripting

Start an agent in the background, give it work, and wait for it:

```sh
argus run -d --name fix-bug claude
argus send --wait --then-wait fix-bug "Fix the failing test"
argus logs --screen fix-bug
```

- `argus send` types only while the agent is `idle` or `done`. Otherwise it
  exits with code 75. Use `--wait` to wait instead.
- `argus wait <agent>...` blocks until every agent named is `free`: its turn
  is over (`idle`, `done` or `quiet`). `--until` takes another availability,
  e.g. `--until exited` for the process to end, which passes on the exit code
  of one agent. `--until-activity` takes an exact activity such as `done`. If a
  named agent exits first, it fails with code `exited`.
- `argus events --json` streams every change as JSON, one object per line.
- `--timeout` exits with code 124.

## Waiting for other agents

`argus status` shows the agents working in a directory and whether they are
all free. It leaves out the agent that runs it, which is always busy running
the command:

```sh
argus status                  # this directory and below
argus status --scope exact    # this directory only
argus status --scope repo     # the whole git repository, all worktrees
argus status --json           # {"summary":{"all_free":true,...},"agents":[...]}
```

`all_free` is true when no running agent is `active`, `attention` or
`unknown`, including when none runs there at all.

To block until that holds, use `argus wait --dir`. It uses the same rules and
picks up agents that start or exit while it waits. `--timeout 0` checks once:
exit 0 if it holds, 124 if not.

```sh
argus wait --dir . --timeout 600 && make release
```

`argus inspect <agent>` shows one agent in full; `--screen` adds its current
screen as plain text.

Agents learn these commands from the instruction argus adds at launch.

Inside an agent, every command that takes an agent accepts `self` for that
agent, e.g. `argus inspect self` or `argus label self title=...`.

## JSON output

Commands print text by default. With `--json` they print JSON, one object per
line:

- Every object has `"schema": 1`. New fields may be added; existing ones do not
  change.
- Every command except `attach`, `grid`, `tree` and `logs` takes
  `--json`.
- `ps`, `label` and `prune` print `{"schema":1,"agents":[...]}`.
- `run -d`, `send`, `rename`, `mv`, `ack` and `rm` print
  `{"schema":1,"agent":{...}}`: the agent after the command (`rm`: before it
  was removed; `run`: plus `warnings`).
- `wait` prints `agent` when waiting on one named agent, `agents` otherwise.
  `wait` and `send --then-wait` keep their exit codes.
- `inspect` prints `agent`, plus `screen` with `--screen` (`null` once
  exited).
- `status` prints `path`, `scope`, `self` (the ID left out, or `null`),
  `summary` (`all_free` and a count per availability) and `agents`.
- `kill` prints `{"schema":1,"ids":[...]}`: the agents signalled, which may
  still be running.
- `manager status` (and `start`, `restart`) prints `running`, `pid`, `build`,
  `stale` and agent counts; `manager stop` prints `was_running`.
- `argus events --json` prints one line per change:
  `{"schema":1,"event":"updated","agent":{...}}`. `event` is `created`,
  `updated`, `exited`, `removed` (with `id` instead of `agent`) or `resync`
  (changes may have been missed; list again).
- An agent has every field `ps` knows plus `group` (`null` at the top level),
  `availability` and `activity_age_secs`.
- `availability` is what to decide on. It is `active` (`working`, `tool:*`,
  `busy`), `free` (`idle`, `done`, `quiet`), `attention` (`blocked`, `error`),
  `unknown`, or `exited`.
- On failure stdout is empty and stderr gets
  `{"schema":1,"error":{"code":"...","message":"..."}}`. Codes: `not_found`,
  `ambiguous`, `not_ready` (exit 75), `timeout` (exit 124), `exited`,
  `manager_unavailable`, `failed`. Other failures exit 1.
