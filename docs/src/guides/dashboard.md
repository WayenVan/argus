# Dashboard

```sh
argus grid     # a live thumbnail of every agent
argus tree     # agents by group, with a detail pane
```

Both take a group prefix and `-l key=value` filters.

| Key | Action |
|---|---|
| arrows / `hjkl` | Move |
| `Enter` | Attach (detach with `Ctrl-\`) |
| `o` | Jump to an existing tmux attachment |
| `Tab` / `[` `]`, `1` `2` | Switch between grid and tree |
| `a` | New agent |
| `r` | Rename |
| `m` | Move to another group |
| `x` | Kill |
| `c` | Copy the name |
| `q` / `Esc` | Quit |

An agent row shows `↗` when it has an attachment in the dashboard's tmux server, or `↗2` for two panes. The detail title shows the total attachment count and how many are in tmux, including other servers. When one reachable tmux pane exists, `o` jumps straight to it; when several exist, choose with the arrow keys and `Enter` (`Esc` cancels). Jumping requires the dashboard and target pane to be in the same tmux server. If multiple tmux clients display the dashboard pane, Argus cannot tell which client pressed `o` and leaves them unchanged. `Enter` always attaches in the current terminal.

Agents whose holders were started by an older Argus version need to be restarted before their tmux attachments can report a pane location.
