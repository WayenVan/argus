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
| `K` | Show the selected agent's details |
| `o` | Jump to an existing tmux attachment |
| `e` | Tree: widen the tree over the detail pane, or restore it |
| `Tab` / `[` `]`, `1` `2` | Switch between grid and tree |
| `a` | New agent |
| `r` | Rename |
| `m` | Move to another group |
| `x` | Kill |
| `c` | Copy the name |
| `q` / `Esc` | Quit |

The details popup shows the same agent information as `argus inspect`, including status, activity, working directory, command, process IDs, and labels. Use `↑`/`↓` or `PgUp`/`PgDn` to scroll, and `K` or `Esc` to close it. In Tree mode, select an agent row first; group headings have no agent details.

Screen previews show the latest visible part of each agent's screen. When attaching from the dashboard to an agent using the normal screen at the same terminal size, Argus restores its current screen and continues from that output position instead of replaying the recent output history. If the sizes differ, it falls back to the history replay. A standalone `argus attach` also replays that history into the terminal's scrollback.

An agent row shows `↗` when it has an attachment in the dashboard's tmux server, or `↗2` for two panes. The detail title shows the total attachment count and how many are in tmux, including other servers. When one reachable tmux pane exists, `o` jumps straight to it; when several exist, choose with the arrow keys and `Enter` (`Esc` cancels). Jumping requires the dashboard and target pane to be in the same tmux server. If multiple tmux clients display the dashboard pane, Argus cannot tell which client pressed `o` and leaves them unchanged. `Enter` always attaches in the current terminal.

Agents whose holders were started by an older Argus version need to be restarted before their tmux attachments can report a pane location.
