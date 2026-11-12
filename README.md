# agent-blame

> To know which model to blame.

Works just like `git blame`, but instead it tells you which model and harness edited that line.

## How it works

`agent-blame` scans your local session histories from multiple agents, and heuristically matches the closest session (or tool call, depending on how that harness stores data) that modified the line. This means it only works for your own agents.

Unconventional edits (scripts, AST edits, etc.) can only be identified if that agent stores file deltas in the session history.
