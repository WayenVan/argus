# Activities

| Activity | Meaning |
|---|---|
| `idle` | Waiting for a prompt. |
| `working` | Running a turn. |
| `tool:<name>` | Running a tool. |
| `blocked` | Waiting for you to approve something. |
| `done` | Finished a turn nobody has looked at yet. It becomes `idle` once you focus the agent, type, or run `argus ack`. |
| `error` | The turn failed. |
| `unknown` | No hooks and no output for 15 s while working, or state lost after a manager restart. |
| `busy` / `quiet` | Programs without hooks: recent output, or none. |
