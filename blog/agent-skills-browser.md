---
title: Add a browser skill to Claude Code and Codex
description: Install the fastbrowse skill so Claude Code or Codex can browse a site and come back with an answer, its quotes and a run status.
topic: Coding agents and MCP
published: 2026-11-01
updated: 2026-11-12
verifiedAgainst: v0.10.0
verifiedOn: 2026-11-12
---

The fastbrowse skill lets a coding agent hand a browser task to fastbrowse and get back an answer, the quotes behind it and a run status it can branch on.

The skill is a set of instructions. It tells the agent when to use fastbrowse, which command or tool to call, which browser to prefer, and how to read a result that is not `complete`. You still configure the model key, and a browser key if the agent may use a cloud browser.

## Install it

The Skills CLI installs it from GitHub:

```sh
npx skills add agent-labs-dev/fastbrowse --skill fastbrowse
```

- The installer asks which agent the skill is for, and installs for the current project by default.
- `--global` installs it for every project.
- `--agent codex claude-code` names the agents without the prompt.

Read the skill before you install it, since it decides when your agent acts on a website.

To install by hand, copy `skills/fastbrowse` to `.claude/skills/fastbrowse` for Claude Code or `.agents/skills/fastbrowse` for Codex, keeping its `references` directory beside `SKILL.md`. Start a new session afterwards, because skills load when a session starts.

## Use it

The agent picks fastbrowse up from the skill's description, or you name it. In Claude Code: `/fastbrowse Find the top story on Hacker News and return its title with a citation.`

The skill follows the browser choice you give it. Otherwise it prefers Browser Use Cloud, which needs `BROWSER_USE_API_KEY`.

For a task that needs your accounts or localhost, name a suitable local browser. A signed-in Chrome can be connected through the fastbrowse extension or its DevTools address. A separate local Chrome is suitable when the task needs no existing sign-in. Browser selection flags depend on the installed CLI version: check `fastbrowse --help` and [the command-line reference](/docs/flags/).

For a list, say whether the task covers the shown records, every page or a numbered range. fastbrowse can follow observed next-page links while checking each page and keeping its citations.

Behind the skill, the agent calls the CLI or an MCP server. Either way the run reports a status. `complete` means the task was verified, and the other statuses say what was missing: an input, an authorization, evidence for a claim, or access to the page.

## Limits and writes

The skill does not authorize writes. For a form, give the agent the URL and the values, and say whether to stop before submitting or to submit. Without that, an irreversible action stops at the confirmation step.

Optional `EXA_API_KEY` adds browserless web search. Its results offer links to open, and its charges count against the run's dollar cap.

The skill sets no spending limits. Pass limits yourself, or put a budget in the task. A cloud browser's charge is added when it stops, separately from any cap on model spend.

[Use fastbrowse with your agent](/docs/skill/) is the install reference. [MCP browser server with spending limits](/blog/mcp-browser-tool/) covers the same tool with its limits fixed on a server.

Next: [your first run](/docs/getting-started/).
