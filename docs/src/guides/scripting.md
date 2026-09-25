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
- An agent that is already free satisfies `wait` at once, even right after a
  `send` it has not picked up yet. To wait for a turn, use turn numbers (below).
- `argus events --json` streams every change as JSON, one object per line.
- `--timeout` exits with code 124.

## Waiting for a turn

Agents with hooks count their finished turns (`turns` in `inspect` and
`--json`). `argus send --json` prints `turn`: the count when the prompt went
in. `argus wait <agent> --after N` waits until the agent is free *and* has
finished a turn after turn N, so a turn that ends before the wait starts is not
missed:

```sh
t=$(argus send fix-bug "Fix the failing test" --json | jq .turn)
# ... other work ...
argus wait fix-bug --after "$t" --timeout 600
```

`argus send --then-wait` does both in one command.

- An agent already past turn N returns at once.
- `--after` takes one agent, and works with `--until` and `--until-activity`,
  but not `--until exited` or `--dir`.
- If the turn ends in `error`, the wait fails with code `stuck`. It also
  fails if the agent goes `unknown` (e.g. interrupted
  with Esc, which ends a turn without a hook). An agent already `unknown` when
  the wait starts, as all are right after `argus manager restart`, does not
  count.
- A `blocked` agent does not fail the wait: agents also report approvals they
  then grant by themselves (Codex does), and argus cannot tell those from a
  prompt waiting on a person. An agent blocked for 2 s or more gets a line on
  stderr, `argus: NAME reports blocked (may be waiting on a person); still
  waiting`, and another when the block ends. A `--timeout` error names the
  activity, e.g. `timed out waiting for fix-bug (blocked)`. Plain `wait`
  prints the same lines. Under `--json`, stderr carries only the error object,
  so there are no such lines.
- Programs without hooks have no turns; `--after` refuses them.
- `send --then-wait` also fails if the agent shows no sign of taking the prompt
  within 5 s.

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
  was removed; `run`: plus `warnings`; `send`: plus `turn`, except with
  `--then-wait`).
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
  `stuck`, `manager_unavailable`, `failed`. Other failures exit 1.
