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

**An agent shows `unknown`.**
It stopped reporting while working, for example after Claude was interrupted
with Esc. See [Claude Code](agents/claude.md#known-limitations).
