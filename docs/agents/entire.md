# Agent session capture

[Entire](https://docs.entire.io) is enabled in this repository, so a commit made during an agent session is linked
to the session behind it: the prompt, the transcript, the tool calls and the files touched. The commit gets an
`Entire-Checkpoint` trailer, the context is stored in local Git refs under `entire/checkpoints`, and no extra commit
lands on the branch. `.entire/settings.json` and the hooks for Claude Code and Codex are committed here.

```sh
curl -fsSL https://entire.com/install.sh | bash   # or: brew install --cask entireio/tap/entire
entire enable                                     # hooks are already configured; this reinstalls them
entire status --detailed
entire checkpoint list --pending                  # what the next commit would carry
entire checkpoint explain <id|sha>
```

Automatic upload of checkpoints is off for this repository (`push_sessions` is false in `.entire/settings.json`),
because the repository is public and a checkpoint carries the prompt and transcript behind a commit. They stay
local until you send them somewhere private, with
`entire configure --local --checkpoint-remote github:<org>/<repo>`.

`uv run pre-commit install` rewrites the Git hooks, so run `entire enable` again after it. `entire agent add cursor`
installs hooks for another agent in the same way. OpenCode is left out: Entire's plugin uses the OpenCode 1 API,
which OpenCode 2 refuses to load ([entireio/cli#2588](https://github.com/entireio/cli/issues/2588)).

The committed hooks run through `sh`. On Windows, run `entire enable` in the checkout so Entire writes the hook form
for the local shell, and leave that rewrite uncommitted.
