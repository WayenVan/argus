# Other programs

Any other program runs unchanged. It has no hooks, so its activity comes from
its output:

- `busy`: it produced output recently.
- `quiet`: it did not.

If the program is a wrapper around a supported agent, pass the agent's kind:

```sh
argus run --kind claude my-claude-wrapper
```
