# Concepts

## Processes

| Process | Role |
|---|---|
| holder | One per agent. Owns the agent's terminal. |
| manager | One per user. Tracks every agent and serves commands. |
| `argus-hook` | Called by the agent's hooks. Reports activity to the manager. |

The holders own the agents, so the manager can stop or restart at any time
without killing them.

## Channels

An agent reaches argus through three separate channels:

```text
holder ──PTY── agent ──pipes── argus-hook ──socket── manager
                 │
                 └──HTTPS── model
```

| Channel | Between | Carries |
|---|---|---|
| PTY | holder and agent | Keystrokes in (typing, `argus send`), screen out (`attach`, `--screen`). The agent's own stdin and stdout. |
| Hook pipes | agent and `argus-hook` | One event as JSON on the hook's stdin. The hook's stdout goes back to the agent. |
| Socket | `argus-hook` and manager | The event, forwarded unchanged. |

The agent program starts a new `argus-hook` for each hook event and connects
it with its own pipes, so hook traffic never appears on the screen. The model
behind the agent sees neither the PTY nor the pipes: it sees the conversation
the agent program sends it. The agent program decides what a hook's stdout
does. For example, Claude Code turns a Stop hook's
`{"decision":"block","reason":"…"}` into a new message and runs the model
again. argus uses this to hold a turn open: see [Labels](#labels).

## Attaching

`attach` shows the agent on the screen it uses. An agent on the alternate
screen (Claude Code, Codex, pi) gets one for the session, and detaching leaves
your terminal as it was. An agent drawing on the normal screen (omp) has
its recent output replayed there, so you can scroll it back. Attaching clears
the terminal's scrollback first, so it holds one copy of that output rather
than one per attach; anything older in that terminal's scrollback is lost.

## Names and groups

An agent's name is a path, such as `web/api/claude`. Every segment except the
last is its group. You can refer to an agent by its ID, by its full path, or
by its last segment if no other agent uses it.

## Activity

The current state of an agent, reported through hooks. See
[Activities](reference/activities.md).

## Labels

Key/value pairs on an agent. Agents set two of them on their own:

- `title`: what the session is about.
- `recap`: what the agent is doing right now.

argus asks agents to do this through an instruction it adds at launch. The
same instruction says what "running", "finished" and "exited" mean for other
agents, and how to check on them, read their results, wait for them and send
them prompts. An agent changes or messages other agents only when you ask. It
starts agents of its own only with your consent, in a group named after
itself, and may then message and stop those without asking.

If a turn ends with `title` or `recap` unset, argus holds it open once and asks
the agent to set them. Only the end that follows counts as the turn, so `wait`
and `send --then-wait` return after it. Claude Code and Codex only; other
agents end the turn as usual.
