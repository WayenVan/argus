<div align="center">

<img src="assets/app-icon.svg" alt="argus" width="112" height="112">

# argus

**Run coding agents in the background and see what each one is doing.**

![Rust](https://img.shields.io/badge/Rust-2024-B7410E?style=flat-square&logo=rust&logoColor=white)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-6289ED?style=flat-square)
![Agents](https://img.shields.io/badge/agents-Claude%20Code%20%C2%B7%20Codex%20%C2%B7%20opencode-8068F2?style=flat-square)
![License](https://img.shields.io/badge/license-MIT-AD99F8?style=flat-square)

<br><br>

<img src="assets/example.png" alt="The argus agent dashboard in a tmux pane, next to an attached Claude Code session" width="100%">

</div>

## Features

- **Always running**: a background daemon starts and manages every agent.
- **Attach anywhere**: attach to any agent from any terminal and detach without stopping it.
- **Live status**: see what every agent is doing as it happens.
- **Jump to tmux**: go straight to the tmux pane where an agent is attached.

## Install

```sh
cargo install --path crates/argus
cargo install --path crates/argus-holder
cargo install --path crates/argus-hook
```

## Use

```sh
argus run claude     # start an agent and attach; detach with Ctrl-\
argus grid           # dashboard of every agent
```

Works with Claude Code, Codex, opencode, and any other terminal program.

## Roadmap

- [ ] Pi driver
- [ ] Kimi Code driver

## Docs

See [`docs/`](docs/src/SUMMARY.md), or build them with `mdbook serve docs`.

## License

[MIT](LICENSE)
