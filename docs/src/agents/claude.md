# Claude Code

Selected when the program is `claude`, or with `--kind claude`.

## How argus wires in

- Hooks are passed with `--settings`. If you pass your own `--settings`,
  argus merges its hooks into a copy of yours.
- The label instructions are passed with `--append-system-prompt`.
- The same settings allow `Bash(argus label self:*)`, so the agent sets its
  own labels without a permission prompt. Other `argus` commands still ask.

No user file is modified.

## Known limitations

**A held turn shows as "Stop hook error".**
When argus holds a turn open to ask for the labels, Claude shows the request
under "Stop hook error". Nothing failed; the agent sets its labels and
finishes.

**Interrupting a turn with Esc shows `unknown`.**
Claude sends no hook when you press Esc. After 15 s without hooks or output,
argus changes `working` to `unknown`. The next prompt fixes it.

**An interrupted turn is missing from `inspect --last`.**
Claude sends no hook when you press Esc, so argus never sees that turn end and
records nothing for it. Its prompt is dropped when the next one arrives.
