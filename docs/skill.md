# Use fastbrowse from Codex or Claude Code

The [fastbrowse skill](../skills/fastbrowse/SKILL.md) lets a coding agent delegate a website task to fastbrowse
and interpret its status and citations. It uses a connected fastbrowse MCP server when available, or the CLI.
Installing the skill installs instructions; the browser and model credentials still need configuration.

## Install

From a checkout of this repository, install the skill for either or both agents with the
[Skills CLI](https://github.com/vercel-labs/skills):

```sh
npx skills add ./ --skill fastbrowse --agent codex claude-code
```

The same installer can fetch the skill from GitHub without a checkout:

```sh
npx skills add agent-labs-dev/fastbrowse --skill fastbrowse --agent codex claude-code
```

The installer uses project scope by default. Add `--global` for all projects, or select just `codex` or
`claude-code`. Review the skill before installing it. For a manual install, copy `skills/fastbrowse` into
`.agents/skills/fastbrowse` for Codex or `.claude/skills/fastbrowse` for Claude Code in the project where you
want to browse. Keep its `references` directory beside `SKILL.md`. Personal installs use
`~/.agents/skills/fastbrowse` or `~/.claude/skills/fastbrowse`. Start a new agent session after installation.

## Configure and use

Install [uv](https://docs.astral.sh/uv/getting-started/installation/) for the CLI path. Set `OPENROUTER_API_KEY`
and `BROWSER_USE_API_KEY` in the calling process environment or a local `.env`, outside the conversation.
For local Chrome, only the model key is required: tell the agent to use local Chrome.
The [CLI guide](../README.md#try-it) covers browser profiles, secrets, and budgets.
For MCP, use the [server setup](../README.md#use-it-from-an-mcp-client) instead; credentials belong to the server's environment.

In Codex:

```text
Use $fastbrowse to find the top story on https://news.ycombinator.com/ and return its title with a citation.
```

In Claude Code:

```text
/fastbrowse Find the top story on https://news.ycombinator.com/ and return its title with a citation.
```

The skill also supports automatic selection from its description. Each delegated task defaults to a
30-step limit and a $0.25 model-spend cap unless you specify a different budget. Cloud browser charges are
added when the browser stops, and are separate from the model-spend cap.

For a form, include its URL and the values to fill. State whether the agent should stop before submission or
submit the specified content. The skill does not authorize writes on its own. A result of `complete` means
the task was verified; other statuses describe the missing input, authorization, evidence, or access.

## Skill design and testing

The skill uses the shared [Agent Skills format](https://agentskills.io/specification). Its description names
the browsing tasks it handles, and its main file links to browser access details when needed. This follows
the [Codex skill guidance](https://learn.chatgpt.com/docs/build-skills) and
[Claude authoring guidance](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices).

Test a skill change in a fresh agent session with real model calls and a local browser. Check the browser's
result as well as the calling agent's answer: a cited read, interactive navigation, a form stopped before
submission, and an explicitly authorized submission. Grade the form against server records, not the agent's
description. Also exercise missing configuration and exhausted budgets so unfinished runs remain unfinished.
Run the documented commands from an installed copy outside this checkout, where repository docs cannot fill
gaps in the skill. Test automatic selection separately from explicit invocation. These are behavioral checks,
not assertions about the skill's wording.
