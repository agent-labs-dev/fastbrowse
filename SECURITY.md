# Security

This document explains how to report a security issue and which versions of fastbrowse
receive fixes.

## Reporting a vulnerability

If you find a security issue in fastbrowse, report it privately. Do not open a public
issue first, because that can expose the problem before a fix is ready.

Report through GitHub Security Advisories on this repository:

https://github.com/agent-labs-dev/fastbrowse/security/advisories/new

Include:

- a short description of the issue
- the version or commit where you found it
- steps to reproduce, when you can share them safely
- any workarounds you know of

We aim to acknowledge a report within a few business days and to publish a fix and an
advisory together. Reporters are credited unless they ask not to be.

## Scope

fastbrowse is a browser agent. The most sensitive surface is how it handles the secrets a
user gives it: API keys, Bitwarden vault logins, and `--secret` values. A valid report is
one where:

- a secret value reaches the LLM, Jev, a log, a recording, a live frame, or the returned result, or
- a secret is sent to an origin it was not authorized for, or
- an irreversible action (a payment, delete, or send) runs without `--authorize`.

Out of scope: missing features, cosmetic problems, and the behavior of the sites the
agent is told to browse.

What a run reports is redacted on its way out, so the places a run is replayed from carry a secret's name in place of
its value: each step in the result's `steps` and in the step events (`on_event`, `serve`'s events, which a host shows
as the run goes), and every trace event (`--trace`, `FASTBROWSE_TRACE`). A typed secret is recorded as `<secret>`.

Live JPEG frames (`on_frame`), MP4 recordings and PNG step frames are held back from the moment a secret is typed, and whenever the page is read showing one, until a reading shows none.
A secret a click reveals (a "show password" toggle) can reach live frames until the next reading. An irreversible action is classified by Jev, so a missed
classification is possible and belongs in a security report.

## Supported versions

Only the latest release receives security fixes. Older releases are not patched. Upgrade
to the latest release before reporting, unless the issue reproduces there too.
