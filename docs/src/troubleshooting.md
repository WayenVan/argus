# Troubleshooting

**A fix or upgrade has no effect.**
Install all three binaries, then run `argus manager restart`. See
[Getting started](getting-started.md#install). argus warns when the manager
runs a different build than the `argus` you invoke, and
`argus manager status` shows both builds plus any agent still on an older
holder. Holders are only replaced when their agent is restarted.

**Activity is never tracked.**
`argus-hook` must be installed next to `argus`. argus warns about this when
you start an agent.

**A Codex agent never updates its title or recap.**
Either the session was started outside argus, or you passed your own
`-c developer_instructions`. See [Codex](agents/codex.md#known-limitations).

**`argus status` says all free while a program is still working.**
Programs without hooks count as free once their output stops. See
[Other programs](agents/generic.md#known-limitations).

**An agent shows `unknown`.**
It stopped reporting while working, for example after Claude was interrupted
with Esc. See [Claude Code](agents/claude.md#known-limitations).

**The mouse wheel does not scroll an attached agent in tmux.**
tmux sends the wheel to programs on the alternate screen instead of entering
copy mode. Agents that use it (Claude Code, Codex, pi) handle it themselves. For
omp, make sure argus is up to date: see [Attaching](concepts.md#attaching).
If an older holder has produced more than 1 MiB of output since the manager
restarted, the manager may have missed its alternate-screen entry. For that
live session, use `argus attach <id> --alt-screen` to request a fresh redraw.
New holders preserve the screen mode when their output ring is truncated.
