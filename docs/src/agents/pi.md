# pi

Selected when the program is `pi`, or with `--kind pi`.

## How argus wires in

argus puts two options in front of your own arguments:

- `-e <extension>` loads argus's extension, which reports activity.
- `--append-system-prompt …` adds the label instructions.

pi accepts both options more than once, so your own `-e` and
`--append-system-prompt` still work, and `--no-extensions` does not disable
argus's extension. No user file is modified.

## Known limitations

**`blocked` is rare.**
pi does not ask before running tools. An agent shows `blocked` only while a
dialog from an extension waits for you during a turn.
`argus inspect` also shows a pending interaction with the dialog kind and
title when pi provides them. Pi's prompt events do not include option labels;
use `argus inspect <agent> --screen` to read those.

**Package commands are not tracked.**
`pi install`, `pi list` and the other package commands run without argus's
options.

**A wrapper script must pass its arguments on.**
With `--kind pi`, argus's options go to your wrapper, which must forward
them to pi.
