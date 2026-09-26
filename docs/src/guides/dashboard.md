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
| `i` | Show the selected agent's details |
| `o` | Jump to an existing tmux attachment |
| `H` `J` `K` `L` | Tree: focus the pane to the left, below, above or right (agents on the left; preview, ID and working directory side by side, title and recap on the right) |
| `Tab` / `Shift-Tab` | Tree: focus the next or previous pane |
| `e` | Tree: zoom the focused agent list or preview over the whole dashboard, or restore it |
| `[` `]`, `1` `2` | Switch between grid and tree |
| `a` | New agent |
| `r` | Rename |
| `m` | Move to another group |
| `x` | Kill |
| `c` | Copy the name; in Tree, what the focused pane shows: the screen, ID, full working directory, title or recap |
| `q` / `Esc` | Quit |

The details popup shows the same agent information as `argus inspect`, including status, activity, working directory, command, process IDs, and labels. Use `↑`/`↓` or `PgUp`/`PgDn` to scroll, and `i` or `Esc` to close it. In Tree mode, select an agent row first; group headings have no agent details.

Screen previews show the latest visible part of each agent's screen. In Tree, focus the preview and use `↑`/`↓` (`k`/`j`) or `PgUp`/`PgDn` to scroll through the rest of its current screen; there is no history beyond it. Keys other than `Enter` act on the focused pane, so switch back to the agent list to select another agent. Attaching from the dashboard to an agent using the normal screen replays its recent output, as `argus attach` does, so you can scroll back through it; each attach adds it to the terminal's scrollback again.

An agent row shows `↗` when it has an attachment in the dashboard's tmux server, or `↗2` for two panes. The detail title shows the total attachment count and how many are in tmux, including other servers. When one reachable tmux pane exists, `o` jumps straight to it; when several exist, choose with the arrow keys and `Enter` (`Esc` cancels). Jumping requires the dashboard and target pane to be in the same tmux server. If multiple tmux clients display the dashboard pane, Argus cannot tell which client pressed `o` and leaves them unchanged. `Enter` always attaches in the current terminal.

Agents whose holders were started by an older Argus version need to be restarted before their tmux attachments can report a pane location.
