# Changelog

All notable changes to argus are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `argus setup` without an agent finds each supported agent on `PATH` and runs
  its setup, asking before each change as `argus setup <agent>` does. `--yes`
  and `--remove` apply to all of them. For Codex it also says whether argus's
  hooks are trusted yet; only Codex can record that, so setup does not.
- `argus inspect --last [N]` prints an agent's last N finished turns: each
  prompt and its full final reply, which `--screen` cuts to what fits on
  screen. The manager records them from hook events into
  `agents/<id>/turns.jsonl` (at most the last 50 turns, compacted to 1 MiB
  once it passes 2 MiB), readable after the agent exits. `argus-hook` cuts a
  prompt or reply to 64 KiB before forwarding it, so a huge pasted prompt can
  no longer push an event past the 1 MiB frame limit and lose it. Claude and Codex report `prompt` and
  `last_assistant_message` themselves; the opencode, pi and omp plugins now
  send the same fields. A turn held back for labels records the held reply
  plus what follows it.

- Claude Code agents that end a turn with `title` or `recap` unset are asked to
  set them before the turn ends. argus's `Stop` hook is now synchronous and
  answers with Claude's `decision: block`; the held end does not count as a
  turn, so `wait` and `send --then-wait` return after the real one. A turn is
  held at most once.
  - `argus-hook` prints the manager's answer to a report. It asks for one only
    from a manager with the new `hook_reply` capability. Clients no longer
    send capabilities in `Hello`, and unknown ones are ignored instead of
    failing the message.
  - Codex agents are held the same way. Its `Stop` hook is synchronous now, so
    Codex asks you to review that one hook again once.
- `argus setup codex` lets Codex agents run `argus label self` and the
  read-only `argus ps`, `status`, `inspect` and `wait` outside the sandbox,
  which blocks the manager's socket. It shows the rules, asks, and writes them
  to `$CODEX_HOME/rules/argus.rules`; `--remove` deletes that file. Starting a
  Codex agent warns while any rule is missing.
- Claude Code agents may run `argus label self …` without a permission prompt:
  argus's settings allow `Bash(argus label self:*)`, added to your own
  `--settings` if you pass one.

### Changed

- The coordination instructions injected at launch are organized by task:
  reading another agent's result (`inspect`, `--screen`), waiting, sending, and
  starting agents of your own, with the user's consent, in a group named after
  you, which the agent may then send to, kill and remove without asking.
- The label instructions are their own section and say why the labels matter:
  the dashboard shows them, and other agents read the recap. They name what
  should and should not change each label, so agents stop relabeling every
  turn. The core section
  now notes that a prompt may come from another agent through `argus send`.

## [0.1.0] - 2026-09-25

### Added

- Catppuccin theming for the TUI, with Mocha or Latte picked from the terminal's
  reported background. The tree selection is a raised surface instead of the
  accent, so activity dots keep their own color, and it fades while the terminal
  window is unfocused.
- Build reporting: `argus-proto` stamps a `BUILD` (crate version plus git
  commit), and the manager and holders return it in their `Hello` replies as an
  optional field older peers ignore.
  - Commands warn once when the manager is a different build, and tree/grid
    flag it in the top bar until it goes away.
  - `argus manager status` shows both builds and lists agents still on an older
    holder.
  - `argus --version` prints the full build.
- A driver for pi. argus loads its own extension with `-e` and adds the label
  instructions with `--append-system-prompt`, ahead of the user's arguments;
  your own `-e` and `--no-extensions` leave them alone. The extension reports
  working, tools, extension dialogs as `blocked`, and the end of each run as
  done, error, or interrupted.
- A driver for omp (oh-my-pi), wired in like pi with its own extension. It
  reports tool approvals and the `ask` tool as `blocked`, including while
  parallel tool calls wait for approval one after another.
- `--json` on every command except `attach`, `grid`, `tree` and `logs`.
  Commands that change agents print them as they are afterwards, so scripts
  need no second lookup. `run --json` needs `-d`.
- `argus status [PATH]`: the agents working in a directory (`--scope under`,
  `exact` or `repo`, the last counting every worktree) and whether they are all
  free. It leaves out the agent running it.
- `argus inspect <agent>`: one agent in full, `--screen` for its current screen
  as plain text.
- The target `self` names the agent running the command, in every command that
  takes an agent. `self` is now a reserved name segment.
- `argus wait` takes several targets (and `group/**`), or `--dir PATH` for
  every agent working there, as `argus status` counts them. A `--timeout`
  error names the agents still pending; `--timeout 0` checks once.
- `argus ps` shows each agent's availability in an `AVAIL` column.
- Turn numbers. Agents with hooks count finished turns (`turns` in `inspect`
  and `--json`, kept across manager restarts), interrupted ones included where
  the agent reports interrupts (codex, opencode, pi, omp). `argus send --json` prints
  `turn`, the count when the prompt went in, and `argus wait <agent> --after N`
  waits until the agent is free after finishing a later turn. Unlike a plain
  `wait`, it cannot return early on a prompt not yet picked up, nor miss a turn
  that ended before it started. It fails with the new code `stuck` if the turn
  ends in an error or the agent goes unknown first. A blocked agent does not
  fail it, since agents also report approvals they grant by themselves;
  waits say on stderr when an agent stays blocked for 2 s and when that ends.

### Changed

- `argus send` also types into an agent in `error`: the turn is over and the
  agent is back at its prompt. Refusals name the availability, e.g. `is active
  (tool:Bash)` or `needs attention (blocked …)`, and say when `--force` helps.
- A manager that stops cleanly notes the output offset of each agent at its
  prompt; the next one gives such an agent its activity back if it printed
  nothing since, instead of leaving it `unknown` until its next hook.
- The instruction argus adds to agents explains `argus send`, and when not to
  use `--force`.

- `--json` output follows one convention. Every object carries `"schema": 1`,
  and agents gain `group`, `availability` (`free`, `active`, `attention`,
  `unknown`, `exited`) and `activity_age_secs`. Under `--json`, errors go to
  stderr as `{"schema":1,"error":{"code","message"}}`.
  - `argus ps --json` prints `{"schema":1,"agents":[...]}` instead of a bare
    array, on one line.
  - `argus events --json` prints `{"schema":1,"event":"updated","agent":{...}}`
    instead of the manager's internal protocol messages.
- `argus wait` now waits until agents are `free` (turn over: `idle`, `done` or
  `quiet`) instead of until they exit; pass `--until exited` for the old
  behavior. `--until` takes only an availability; exact activities move to
  `--until-activity`. A named agent that exits before reaching the state fails
  with code `exited` instead of `failed`.
- The instruction argus adds to agents at launch now allows read-only commands
  for coordinating with other agents and explains how to check on and wait for
  them: the states, the commands and their pitfalls. It says that other agents the user refers to
  as running now are argus agents, to be checked with argus rather than the
  agent's own subagent or session tools, but questions about agent programs or
  code are not. It says that "running" means `active`,
  "finished" means `free`, only "exited" means the process ended, and "agents"
  leaves out exited ones. Commands that change or message other agents still
  need the user to ask.
- `argus send --then-wait` waits for the turn its prompt started, by turn
  number, instead of guessing from activity changes. An errored or
  interrupted turn now fails with code `stuck` (still exit 1) instead of
  printing the activity; a blocked one keeps waiting.
- The label instruction is shorter, gives examples, and tells agents to
  single-quote values on one line (`argus label self title='...'`): double
  quotes let the shell expand `$` and backticks in a recap.

### Fixed

- A new Codex agent no longer shows `idle` while a startup dialog (trust this
  folder, review hooks) is up, so `argus send` cannot answer it: Codex draws
  its prompt first and the dialog over it a second later. It is `unknown`
  until its prompt has kept a cursor for 3 s.
- `argus send` types escape sequences and control characters (Esc, arrow
  keys, Ctrl-C) as keys again instead of pasting them as text.

- `argus send` to Codex left the prompt in its input box instead of
  submitting it: Codex took the fast typing for a paste and the Enter after it
  as a new line. Single-line prompts are now sent as a bracketed paste too,
  when the agent accepts pastes.
- `attach` draws on the screen the agent is on. It used to always enter its
  own alternate screen, so in tmux the mouse wheel did nothing for agents that
  draw on the normal screen, such as omp — unless the agent had toggled the
  alternate screen itself since, as omp does on every resize. Such agents now
  scroll with tmux copy mode or the terminal's scrollback, and their output
  stays there after detaching; agents on their alternate screen are unchanged.
- `argus wait --help` gave `waiting` as an example activity; no agent reports
  it. It now says `idle`.
- Keep screen tracking alive when vt100 panics: vendor vt100 0.16.2 with a fix
  for a panic after a row is shortened through a wide character (a resize or an
  ICH) and that column is then written or erased (see `vendor/vt100/PATCHES.md`).
  - Each agent's screen state now has its own lock, so a panic poisons only that
    agent instead of the manager's single screen lock.
  - A screen tracker that panicked or found its state poisoned is restarted,
    backfilling from the holder's ring buffer.
  - The TUI's preview connection reconnects when it breaks.

## [0.0.1] - 2026-09-25

### Added

- Initial release: a background manager, `argus-holder` to keep each agent
  alive, and `argus-hook` to report what an agent is doing.
- Drivers for Claude Code, Codex, and opencode, plus a generic terminal driver.
- `argus run`, `argus attach`, and `argus grid`/`tree` dashboards with
  rename/move/kill/copy/new-agent shortcuts and tmux pane jumps.

[Unreleased]: https://github.com/WayenVan/argus/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/WayenVan/argus/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/WayenVan/argus/releases/tag/v0.0.1
