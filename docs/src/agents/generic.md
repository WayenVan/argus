# Other programs

Any other program runs unchanged. It has no hooks, so its activity comes from
its output:

- `busy`: it produced output recently.
- `quiet`: it did not.

If the program is a wrapper around a supported agent, pass the agent's kind:

```sh
argus run --kind claude my-claude-wrapper
```

## Known limitations

**`quiet` counts as free.**
- *What happens:* `argus status` and `argus wait` treat a program
  that has printed nothing for about 2 s as free, even if it is still working.
- *Why:* without hooks, output is the only signal.
- *What to do:* wrap the program with `--kind` if it is a supported agent, or
  wait for it by name with `--until exited`.

**Briefly `busy` after starting.**
Startup output shows as `busy` for a second or two before it settles.
