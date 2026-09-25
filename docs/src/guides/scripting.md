# Scripting

Start an agent in the background, give it work, and wait for it:

```sh
argus run -d --name fix-bug claude
argus send --wait --then-wait fix-bug "Fix the failing test"
argus logs --screen fix-bug
```

- `argus send` types only while the agent is `idle` or `done`. Otherwise it
  exits with code 75. Use `--wait` to wait instead.
- `argus wait <agent> --until <activity>` blocks until the agent reaches that
  activity, or until it exits (the default).
- `argus events --json` streams every change as JSON, one object per line.
- `--timeout` exits with code 124.
