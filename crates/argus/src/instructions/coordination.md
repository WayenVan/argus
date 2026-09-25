# Checking on and waiting for other argus agents

These commands only read; add --json to any of them to parse the output. Do not run commands
that change or message other agents (send, kill, rm, label, rename, mv, ack) unless the user
explicitly asks you to. `self` names you in every command that takes an agent.

## States

Every agent has three, from coarse to fine:

- status: whether the process runs, running or exited.
- availability: what to decide on.
  - free: its turn is over (activity idle, done or quiet); the process keeps running.
  - active: doing something (working, tool:<name>, busy).
  - attention: waiting on a person (blocked on a permission prompt, or error).
  - unknown: no reliable signal.
  - exited: the process ended.
- activity: the detail, e.g. idle, done, working, tool:Bash, blocked.

Decide on availability. Unless the user says otherwise:

- "running", "working" or "busy" means active.
- "finished", "done" or "ready" means free.
- only "quit", "closed" or "exited" means exited.
- "agents" means running ones; exited agents are left out unless the user asks about them.

## Commands

  argus status                     agents working in this directory and below, not you;
                                   summary.all_free: none is active, attention or unknown
  argus ps [-a]                    every agent with its AVAIL; -a adds exited ones
  argus inspect <id> [--screen]    one agent in full; --screen adds its screen as text
  argus wait <id>... --timeout <secs>
                                   block until each named agent is free
  argus wait <id> --until exited --timeout <secs>
                                   block until its process ends; exits with its exit code
  argus wait --dir . --timeout <secs>
                                   block until every agent here is free, counting agents
                                   that start while it waits
  argus wait <id> --until-activity <activity> --timeout <secs>
                                   block until one exact activity, e.g. done
  argus wait <id> --after <n> --timeout <secs>
                                   block until it is free after finishing a turn past
                                   turn n (inspect shows its turns)

## Waiting

- Always pass --timeout and keep it under your shell tool's time limit; exit 124 means it
  timed out. --timeout 0 checks once. If your shell tool can run a command in the background
  and notify you when it ends, use that for long waits.
- An agent that is free already returns at once, unless you pass --after.
- A wait keeps going while an agent is blocked, since agents also report approvals they then
  grant by themselves. If it stays blocked, the wait says so on stderr ("reports blocked") and
  when that ends; tell the user then instead of waiting it out, as it may need them.
- A named agent that exits before becoming free fails the wait (error code exited).
- Programs without hooks (any kind but claude, codex, opencode, pi and omp) count as free
  once they stop printing, even while still working; wait for them with --until exited.

## Sending, only when the user asks

  argus send <id> '<prompt>' --then-wait --timeout <secs>
                                   type a prompt and wait for the turn it starts;
                                   --wait first waits until the agent can take one

- A refusal says why (active, blocked, unknown). Do not retry with --force unless the user
  says so or `argus inspect <id> --screen` shows it at an empty prompt; argus never types
  into a blocked agent.
