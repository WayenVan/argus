# Claude Code

Selected when the program is `claude`, or with `--kind claude`.

## How argus wires in

- Hooks are passed with `--settings`. If you pass your own `--settings`,
  argus merges its hooks into a copy of yours.
- The label instructions are passed with `--append-system-prompt`.

No user file is modified.

## Known limitations

**Interrupting a turn with Esc shows `unknown`.**
Claude sends no hook when you press Esc. After 15 s without hooks or output,
argus changes `working` to `unknown`. The next prompt fixes it.
