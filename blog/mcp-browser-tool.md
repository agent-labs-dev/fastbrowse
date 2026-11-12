---
title: MCP browser server with spending limits
description: fastbrowse-mcp serves one browse tool over MCP with its limits set on the server. What each flag fixes, and what a calling model can and cannot change.
topic: Coding agents and MCP
published: 2026-11-01
updated: 2026-11-12
verifiedAgainst: v0.9.1
verifiedOn: 2026-11-12
---

`fastbrowse-mcp` serves one `browse` tool over MCP. Its limits are set when the server starts. A call can ask for less, never more.

## Flags the server fixes

A calling model cannot override these:

| Flag | Effect |
|:--|:--|
| `--local`, `--headed`, `--profile DIR`, `--downloads DIR`, `--cursor`, `--no-cursor` | fixed for every call; `--local` starts separate Chrome, and `--headed` or `--profile` imply it |
| `--cloud-profile ID` | every call runs as that cloud profile |
| `--allow-authorize` | lets a call pass `authorize`; without it an irreversible action always stops at `needs_confirmation` |
| `--secret NAME=ENV_VAR@ORIGIN` | typed when a call's start page is on `ORIGIN`; the model sees `NAME` only |
| `--bitwarden ITEM` | a vault login a call may name |
| `--max-steps N`, `--max-dollars N`, `--max-seconds N` | ceilings per call, unlimited by default |
| `--max-concurrent N` | runs at once, default 1; further calls wait |

With `--allow-authorize` off, every irreversible action stops for confirmation. With the three ceilings set, you know the most a call can cost before the first one runs.

## Stdio or HTTP

Over stdio, the server runs for as long as the client that launched it. With `--transport http` it serves streamable HTTP at `/mcp`. Every request except `/healthz` then needs the bearer token in `FASTBROWSE_MCP_TOKEN`, and the server will not bind beyond loopback without one.

## Add it to a client

```sh
claude mcp add fastbrowse -e TYPESAFE_API_KEY=... -e OPENROUTER_API_KEY=... -e BROWSER_USE_API_KEY=... \
  -- uvx --from fastbrowse fastbrowse-mcp --max-dollars 0.25
```

A client configured by JSON, such as Claude Desktop, uses the same command, `uvx --from fastbrowse fastbrowse-mcp`, with those keys in its `env`. A run takes seconds to minutes, so raise the client's tool timeout. In Claude Code that setting is `MCP_TOOL_TIMEOUT`.

## What the tool returns

The tool returns the answer with numbered citation links, typed `fields` if the call asked for them, the quotes behind those fields, the run status and a next step. It reports progress at every step. The status tells the model whether the answer was verified.

[Use it from an MCP client](/docs/mcp/) is the server reference. [Add a browser skill to Claude Code and Codex](/blog/agent-skills-browser/) covers the skill route, and [the safety model](/docs/safety/) explains what the authorization gate judges.

Next: [your first run](/docs/getting-started/).
