# Chrome Web Store automation

The Extension Store workflow builds the extension from the selected commit. It removes the development
manifest key, checks its identity against the existing Store item and records the package version and SHA256.
The extension version comes from its manifest, independently of the CLI version.

Changes to the extension on `main` build the ZIP and submit it for staged review. CLI release tags build it
and verify that the manifest version is already published to all users, without submitting again.
Submitting an already published version fails and requires a manifest version bump. An existing submission blocks another upload: use
the workflow's `status` action to inspect it. Google review can outlast a workflow run.

Run the workflow manually with `build` for a package without Store access, `status` for the current Store
response, `submit` to upload and request review, or `publish` to publish the approved, staged version.
Remote actions run only from `main` or a release tag, through the `chrome-web-store` environment.
The package artifact and Store response artifact retain the result of each successful operation.

## Publisher setup

1. Enable the Chrome Web Store API in the publisher's Google Cloud project. Create a dedicated service
   account and link its email in the existing Chrome Web Store publisher's Account section. The publisher
   ID is shown under Publisher > Settings. Keep the existing extension item.
2. Configure Google Workload Identity Federation for this repository's Extension Store workflow and
   `chrome-web-store` environment. Restrict the provider's attribute condition to this repository and
   environment, and its permitted source refs. Grant that identity permission to impersonate the dedicated account
   for short-lived access tokens. Do not create a service-account key.
3. Configure the GitHub environment's variables `CWS_PUBLISHER_ID`, `CWS_SERVICE_ACCOUNT` and
   `CWS_WORKLOAD_IDENTITY_PROVIDER`. Use the environment's reviewer and deployment rules to control
   Store access. Never place access tokens in repository variables or files.
4. Dispatch `status` first. A successful response must identify item
   `ghodfgfhbfaghhfdabopagbfopdjgpop`. Resolve authentication or publisher access errors before submitting.
5. Dispatch `submit` from the commit whose package was reviewed. Submission uses `STAGED_PUBLISH`,
   does not skip Google's review and blocks on validation warnings. Inspect `status` for approval.
6. Dispatch `publish` from the same source when its manifest version is approved and staged. The script
   refuses a different version or a pending review. Verify the approved Store installation in a clean
   branded Chrome profile through the current CLI and a completed task before announcing compatibility.

The CLI release owner must require the compatible Store version before publishing a CLI that needs it.
That release job needs the same environment, OIDC permission and authentication step as the Store job.
It must call the check directly, rather than rely on the separate Store workflow completing.
`python3 scripts/extension_store.py verify --version VERSION` checks the published version and full rollout
with the same authentication environment as `status`. It does not replace the installed-extension task check.
After an HTTP timeout or interrupted write, inspect `status` before retrying: an operation may have reached
Google even if the workflow did not receive its response. The script never retries a write automatically.
An older hotfix tree whose manifest names an older extension fails verification after the newer extension
is published. Bring the compatible extension source forward before releasing that tree.

The script reads `CWS_ACCESS_TOKEN` only for Store operations. The workflow obtains it through OIDC with
the `https://www.googleapis.com/auth/chromewebstore` scope and does not store a long-lived credential.

API contracts: [publishing workflow](https://developer.chrome.com/docs/webstore/using-api),
[service accounts](https://developer.chrome.com/docs/webstore/service-accounts),
[staged publication](https://developer.chrome.com/docs/webstore/api/reference/rest/v2/publishers.items/publish).
