# Activities

| Activity | Availability | Meaning |
|---|---|---|
| `idle` | `free` | Waiting for a prompt. |
| `working` | `active` | Running a turn. |
| `tool:<name>` | `active` | Running a tool. |
| `blocked` | `attention` | Waiting for you to approve something. |
| `done` | `free` | Finished a turn nobody has looked at yet. It becomes `idle` once you focus the agent, type, or run `argus ack`. |
| `error` | `attention` | The turn failed. |
| `unknown` | `unknown` | No hooks and no output for 15 s while working, or state lost across a manager restart (see below). |
| `busy` / `quiet` | `active` / `free` | Programs without hooks: recent output, or none. |

## Availability

The activity, coarsened to what a script or another agent decides on: `free`,
`active`, `attention`, `unknown`, or `exited` once the agent stops. `argus ps`
shows it as `AVAIL`; `argus status`, `argus wait` and every `--json` agent use
it. `free` is not the `idle` activity: `done` and `quiet` are free too.

`quiet` counts as `free`, but a program without hooks is only quiet: it may be
thinking. `argus send` still refuses it without `--force`.

## Across a manager restart

Hooks cannot reach a stopped manager, so a new manager starts every agent as
`unknown`. When the old one stopped cleanly (`argus manager stop` or
`restart`, or SIGTERM), it noted how much output each agent at its prompt
(`idle`, `done`, `error`) had printed. An agent that printed nothing since gets
its activity back, `since` time included; any other stays `unknown` until its
next hook. A crashed manager notes nothing.
