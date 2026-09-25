# opencode

Selected when the program is `opencode`, or with `--kind opencode`.

## How argus wires in

argus adds a plugin and the label instructions through
`OPENCODE_CONFIG_CONTENT`. If you set that variable yourself, argus merges
its config into yours. No user file is modified, and nothing needs trusting.

## Known limitations

**No activity until the first prompt.**
opencode creates its session with the first prompt.

**Input is not accepted for the first few seconds.**
opencode ignores keys typed while it starts, about 5 s for opencode 1.18.
Until its prompt shows a cursor the agent is `unknown`, so `argus send`
refuses it; `argus send --wait` waits until it is `idle`.
