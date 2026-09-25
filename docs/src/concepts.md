# Concepts

## Processes

| Process | Role |
|---|---|
| holder | One per agent. Owns the agent's terminal. |
| manager | One per user. Tracks every agent and serves commands. |
| `argus-hook` | Called by the agent's hooks. Reports activity to the manager. |

The holders own the agents, so the manager can stop or restart at any time
without killing them.

## Attaching

`attach` shows the agent on the screen it uses. An agent on the alternate
screen (Claude Code, Codex, pi) gets one for the session, and detaching leaves
your terminal as it was. An agent drawing on the normal screen (omp) has
its recent output replayed there, so you can scroll it back; it stays in your
scrollback after you detach, and each attach adds it again.

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

argus asks agents to do this through a short instruction it adds at launch.
The same instruction says what "running", "finished" and "exited" mean for
other agents, how to check on and wait for them, and not to change or message
other agents unless you ask.
