# Config file

argus reads `$ARGUS_CONFIG`, else `$XDG_CONFIG_HOME/argus/config.toml`, else
`~/.config/argus/config.toml`. No file means the defaults below. Unknown keys
and bad values are errors, reported by the next command.

```toml
default_group = "work"
detach_key = "ctrl-b"

[profiles.review]
program = "claude"
args = ["--permission-mode", "plan"]
group = "review"
labels = { role = "reviewer" }

[profiles.cc]          # a wrapper script for Claude Code
kind = "claude"

[dashboard]
group_from_tmux = true

[manager]
silence = "30s"
replay_buffer = "4M"
```

## Top level

| Key | Default | Purpose |
|---|---|---|
| `default_group` | none | Group for `argus run` without `--in` or `$ARGUS_GROUP`. |
| `detach_key` | `"ctrl-]"` | Ends `argus attach`. `ctrl-` plus a letter, `\` or `]`; not `h`, `i`, `j` or `m`. |

## Profiles

`argus run <name>` uses `[profiles.<name>]` when there is one. `--no-profile`
skips it.

| Key | Purpose |
|---|---|
| `program` | Program to run. Default: the profile's name. |
| `args` | Put before the arguments after `--`. |
| `kind` | Driver to use, as `--kind`. `--kind` wins. |
| `group` | Used after `--in` and `$ARGUS_GROUP`, before `default_group`. |
| `labels` | Added to the agent. `--label` wins for the same key. |

A profile named after a program, such as `[profiles.claude]`, changes what
`argus run claude` starts. The TUI's `a` (new agent) does not use profiles yet.

## `[dashboard]`

| Key | Default | Purpose |
|---|---|---|
| `group_from_tmux` | `false` | `a` prefills `--in` with the dashboard's tmux session name instead of the group under the cursor, in Tree and Grid. The name is lowercased, other characters become `-`, and one that is all digits (tmux's default `0`) becomes `tmux-0`. Outside tmux, `a` behaves as without it. |

## `[manager]`

Read when the manager starts: run `argus manager restart` after a change.
`kill_grace` and `replay_buffer` apply to agents started after that.

| Key | Default | Purpose |
|---|---|---|
| `silence` | `"15s"` | `working` with no hooks and no output this long becomes `unknown`. |
| `typing_grace` | `"10s"` | `argus send` refuses this long after someone types in an attached terminal. |
| `turn_history` | `50` | Finished turns kept for `inspect --last` (also capped at about 1 MB). |
| `kill_grace` | `"5s"` | `argus kill`: SIGTERM, then SIGKILL after this. |
| `replay_buffer` | `"1M"` | Recent output kept per agent for `attach --replay` and `logs`. 64K to 64M. |

Durations take `ms`, `s`, `m` or `h`. Sizes are bytes or `K` / `M`.
