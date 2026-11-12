---
title: Browser agent logins without sharing passwords
description: "Three ways to let a fastbrowse run act in your account: your own signed-in Chrome, a cloud profile, or a login typed from a Bitwarden vault."
topic: Browsers and logins
published: 2026-11-01
updated: 2026-11-12
verifiedAgainst: v0.10.0
verifiedOn: 2026-11-12
---

fastbrowse offers three ways to let a run act in your account. In order of how much you hand over:

1. Your own Chrome, already signed in.
2. A cloud profile someone signed into once.
3. A secret typed from a vault when a field needs it.

## Use the Chrome you are signed in to

The command line runs a browser task in your own Chrome through the [fastbrowse extension](https://chromewebstore.google.com/detail/fastbrowse/ghodfgfhbfaghhfdabopagbfopdjgpop), so every site you are signed in to is already open to the run. Set it up once: run `fastbrowse extension install`, then add the extension from the Chrome Web Store.

```sh
fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." --start https://www.amazon.com/
```

The agent never receives a password, because the session is already in your Chrome. With no extension connected, the run ends `unavailable` before it spends anything.

To keep a signed-in profile apart from your everyday Chrome, use an SDK. The command line refuses `--profile` and `--headed`, and the Python SDK takes them in `LocalChrome`. Sign in on the first run, then reuse the directory:

```python
from pathlib import Path

from fastbrowse import run_task
from fastbrowse.models import LocalChrome

await run_task(
    "Add a UGREEN USB-A to USB-C cable, 2m, to my cart.",
    start="https://www.amazon.com/",
    chrome=LocalChrome(profile=Path.home() / ".fastbrowse" / "amazon", headed=True),
)
```

`headed=True` is needed for the first run only. A Chrome that is already running works too, over its DevTools port from an SDK: see [connect an agent to a running Chrome over CDP](/blog/attach-to-running-chrome/).

## Use a cloud profile

With `--cloud`, `--cloud-profile` runs as a profile someone signed into once. The run inherits its cookies, and no model sees a credential. A cloud browser is billed for its time, separately from any cap on model spend.

## Type the login from a vault

With the Bitwarden CLI unlocked, `--bitwarden ITEM` types the vault login for that item.

- The item's saved URIs must match `--start`.
- The model sees only the names `username`, `password` and, when the item holds an authenticator key, `one_time_code`, which is computed as it is typed.

```sh
export BW_SESSION="$(bw unlock --raw)"
fastbrowse "Add a UGREEN USB-A to USB-C cable, 2m, to my cart." \
  --start https://www.amazon.com/ --bitwarden Amazon
```

## How secrets are handled

Whether a value comes from a vault or the environment:

- Models see names only. The value is resolved when it is typed.
- A value is typed only on the origin it was declared for.
- A value is redacted from the result, events and traces.

A password field is typed only from a stored secret. To change a password, add the new one as an origin-scoped secret named `new_password`, refer to it in the task, and pass `--authorize`.

[Signed-in sites and secrets](/docs/signed-in-sites/) is the reference for profiles, vault items and secrets. [Command-line flags](/docs/flags/) lists `--secret` and `--authorize`, and [the safety model](/docs/safety/) explains the authorization gate and its limits.

Next: [your first run](/docs/getting-started/).
