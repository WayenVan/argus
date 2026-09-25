# argus

Run coding agents in the background and see what each one is doing.

```sh
cargo install --path crates/argus
cargo install --path crates/argus-holder
cargo install --path crates/argus-hook

argus run claude     # start an agent and attach; detach with Ctrl-\
argus grid           # dashboard of every agent
```

Supports Claude Code, Codex, opencode, and any other terminal program.

Documentation: [`docs/`](docs/src/SUMMARY.md). Build it with `mdbook serve docs`.

MIT licensed.
