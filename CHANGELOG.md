# Changelog

All notable changes to argus are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

### Fixed

- `attach` draws on the screen the agent is on. It used to always enter its
  own alternate screen, so in tmux the mouse wheel did nothing for agents that
  draw on the normal screen, such as omp — unless the agent had toggled the
  alternate screen itself since, as omp does on every resize. Such agents now
  scroll with tmux copy mode or the terminal's scrollback, and their output
  stays there after detaching; agents on their alternate screen are unchanged.
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

[Unreleased]: https://github.com/WayenVan/argus/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/WayenVan/argus/releases/tag/v0.0.1
