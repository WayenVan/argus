# Codex

Selected when the program is `codex`, or with `--kind codex`.

## How argus wires in

- Hooks are passed as `-c hooks.<Event>=…` overrides. All run in the
  background except `Stop`, whose answer Codex waits for, so argus can hold a
  turn that ends without labels (see [Labels](../concepts.md#labels)).
- The label instructions are passed as `-c developer_instructions=…`.

No user file is modified.

## First run: trust the hooks

Codex runs a hook only after you trust it. On the first run, attach to the
agent and accept argus's hooks. You only need to do this once, and again for
the `Stop` hook after upgrading from 0.1.0, which ran it in the background.

## Known limitations

**The sandbox stops the agent from setting its labels until you allow it.**
`argus label` connects to the manager's socket, which Codex's sandbox blocks
(`Operation not permitted`). Without a rule, each label update needs your
approval, or fails, depending on your approval settings. argus warns when it
starts a Codex agent while the rule is missing. Allow it once:

```sh
argus setup codex            # shows the rule, asks, then writes it
argus setup codex --remove   # undo
```

It writes one line to `$CODEX_HOME/rules/argus.rules` (default
`~/.codex/rules/`), argus's own file next to yours:

```text
prefix_rule(pattern=["argus", "label", "self"], decision="allow")
```

- *Scope:* exactly `argus label self …` runs outside the sandbox without a
  prompt. A chained command such as `argus label self x && other` still runs
  sandboxed. The rule matches the word `argus`, so a call by absolute path
  (`/usr/local/bin/argus label self …`) is not covered.
- *Reach:* every Codex session reads it, not only agents argus started. Outside
  argus the command has no agent to label and fails.
- *Why a file:* Codex reads rules only from its rules directory. Claude takes
  its permission per session through `--settings`, so it needs no setup.

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
  exists, set the labels yourself with `argus label`.

Sessions started in argus keep the instructions when you resume them, in
argus or anywhere else.
