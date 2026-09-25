# omp

Selected when the program is `omp` (oh-my-pi), or with `--kind omp`.

## How argus wires in

As with [pi](pi.md), argus puts two options in front of your own arguments:

- `-e <extension>` loads argus's extension, which reports activity.
- `--append-system-prompt …` adds the label instructions.

omp accepts both options more than once, so your own `-e` and
`--append-system-prompt` still work, and `--no-extensions` does not disable
argus's extension. No user file is modified.

An agent shows `blocked` while omp waits for a tool approval or for your
answer to its `ask` tool.

## Known limitations

**Subcommands get the options too.**
argus also puts its options in front of `omp commit` and omp's other
subcommands; omp drops them there. A bare management word omp would reject
(`omp list`) is sent to the model as a prompt instead.

**`/new` does not clear `done`.**
Starting a new session inside omp leaves an unseen `done` in place, as with
every agent. Attach or run `argus ack` to clear it.

**A wrapper script must pass its arguments on.**
With `--kind omp`, argus's options go to your wrapper, which must forward
them to omp.
