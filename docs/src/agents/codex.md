# Codex

Selected when the program is `codex`, or with `--kind codex`.

## How argus wires in

- Hooks are passed as `-c hooks.<Event>=…` overrides. All run in the
  background except `Stop`, whose answer Codex waits for, so argus can hold a
  turn that ends without labels (see [Labels](../concepts.md#labels)).
- The label instructions are passed as `-c developer_instructions=…`.
- Activity, turns, and pending interactions follow the main session. A `/btw`
  side conversation has its own session ID and does not replace that state.
- A permission hook records an `observed` request. It fires before Codex's
  automatic reviewer, so it cannot prove a human prompt is visible. Argus
  promotes it to `needs_user` (and the agent to `blocked`) once Codex's
  approval prompt appears on screen, recognized by its list of choices, key
  hints, enter/esc footer, and the requested command. The next hook event
  clears it. If a request stays `observed`, use `argus inspect <id> --screen`
  to see whether a person must answer.

No user file is modified.

## First run: trust the hooks

Codex runs a hook only after you trust it. On the first run, attach to the
agent and accept argus's hooks. You only need to do this once, and again for
the `Stop` hook after upgrading from 0.1.0, which ran it in the background.
`argus setup` and `argus setup codex` tell you while they are not trusted yet.
Only Codex can record the trust, so setup does not write it.

## Known limitations

**The sandbox blocks `argus` commands until you allow them.**
`argus label` and the read-only `argus ps`, `status`, `inspect` and `wait`
connect to the manager's socket, which Codex's sandbox blocks
(`Operation not permitted`). Without rules, each call needs your approval, or
fails, depending on your approval settings. argus warns when it starts a Codex
agent while the rules are missing. Allow them once:

```sh
argus setup codex            # shows the rules, asks, then writes them
argus setup codex --remove   # undo
```

It writes these lines to `$CODEX_HOME/rules/argus.rules` (default
`~/.codex/rules/`), argus's own file next to yours:

```text
prefix_rule(pattern=["argus", "label", "self"], decision="allow")
prefix_rule(pattern=["argus", "ps"], decision="allow")
prefix_rule(pattern=["argus", "status"], decision="allow")
prefix_rule(pattern=["argus", "inspect"], decision="allow")
prefix_rule(pattern=["argus", "wait"], decision="allow")
```

- *Scope:* exactly these commands run outside the sandbox without a prompt.
  `send`, `kill`, `rm` and the rest still ask. A chained command such as
  `argus ps && other` still runs sandboxed. The rules match the word `argus`,
  so a call by absolute path (`/usr/local/bin/argus ps`) is not covered.
- *Reach:* every Codex session reads them, not only agents argus started.
- *Why a file:* Codex reads rules only from its rules directory. Claude takes
  its permission per session through `--settings`, so it needs no setup.
- *Upgrading:* a rules file from an older `argus setup codex` holds only the
  label rule; run it again to add the rest.

**No hook until the first prompt.**
Codex fires its first hook with the first prompt. A new agent is `unknown`
until its prompt has shown a cursor for 3 s, then `idle`. Codex draws its
prompt first and a startup dialog (trust this folder, review hooks) over it
about a second later; while a dialog is up the agent stays `unknown`, so
`argus send` does not answer it. Answer it by attaching. A dialog that appears
later than 3 s is not caught.

**Your own `-c developer_instructions` replaces argus's.**
Codex uses the last value it is given. The agent still runs, but it will not
update its labels.

**Your own `-c hooks.*` may replace argus's hooks.**
argus warns when you start the agent.

**Resuming a session started outside argus: no labels.**
- *What happens:* the agent never sets `title` or `recap`. Activity tracking
  still works.
- *Why:* Codex stores `developer_instructions` in the session when the
  session is created. On resume, it replays the stored session and ignores
  new `-c developer_instructions`.
- *What to do:* start the session in argus. For a session that already
  exists, set the labels yourself with `argus label`, or tell the agent to
  run `argus guide` and follow it.

Sessions started in argus keep the instructions when you resume them, in
argus or anywhere else.
