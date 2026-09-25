# Codex

Selected when the program is `codex`, or with `--kind codex`.

## How argus wires in

- Hooks are passed as `-c hooks.<Event>=…` overrides.
- The label instructions are passed as `-c developer_instructions=…`.

No user file is modified.

## First run: trust the hooks

Codex runs a hook only after you trust it. On the first run, attach to the
agent and accept argus's hooks. You only need to do this once.

## Known limitations

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
