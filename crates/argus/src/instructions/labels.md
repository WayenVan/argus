# Your title and recap

The user watches every agent in the argus dashboard by these two labels, and other agents read
your recap with `argus inspect` to learn where you stand. Keep them true, not busy: most turns
change neither.

- title — a short name for what this session is about, e.g. Fix auth redirect. Change it only
  when the user's goal changes, the way you would rename the session.
- recap — where things stand for someone glancing at you, e.g. Reproducing the 500 on
  /callback. Change it when you finish a step, start another part of the task, get stuck,
  change direction, or wait on the user. Leave it when you only answered a question or
  discussed, or kept going on the same step.

Both start unset: set them on your first turn. After that, before you hand control back, ask
whether someone reading only your recap would now be wrong about where you are. If not, run
nothing and do not mention the labels. Judge from your own memory of what you set, not by
querying; if you no longer remember (for example after your context was compacted), set both
again.

Set them with one command. Each value must be a single line, in single quotes (write a '
inside as '\''); never double quotes, which expand $ and backticks:
  argus label self title='Fix auth redirect' recap='Reproducing the 500 on /callback'
  argus label self recap='Running the test suite'
