"""Build the Store package and manage its staged Chrome Web Store submission.

API contract: https://developer.chrome.com/docs/webstore/using-api
Only submit and publish write to the Store; build and status never publish.
"""

import argparse
import base64
import hashlib
import json
import os
import re
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates/fastbrowse-browser/extension"
ITEM = "ghodfgfhbfaghhfdabopagbfopdjgpop"
API = "https://chromewebstore.googleapis.com"
FILES = (
    "allowed-methods.json",
    "background.js",
    "icon-16.png",
    "icon-32.png",
    "icon-48.png",
    "icon-128.png",
    "manifest.json",
    "popup.html",
    "popup.js",
)


def build(source: Path, output: Path) -> dict[str, Any]:
    manifest = json.loads((source / "manifest.json").read_text())
    digest = hashlib.sha256(base64.b64decode(manifest.pop("key"), validate=True)).digest()[:16].hex()
    identity = digest.translate(str.maketrans("0123456789abcdef", "abcdefghijklmnop"))
    if identity != ITEM:
        raise ValueError("the source key does not identify the existing Store item")
    output.parent.mkdir(parents=True, exist_ok=True)
    with ZipFile(output, "w", compression=ZIP_DEFLATED) as archive:
        for name in sorted(FILES):
            path = source / name
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"unexpected package entry: {path.name}")
            payload = (
                json.dumps(manifest, indent=2).encode() + b"\n" if path.name == "manifest.json" else path.read_bytes()
            )
            entry = ZipInfo(path.name, (1980, 1, 1, 0, 0, 0))
            entry.compress_type = ZIP_DEFLATED
            entry.external_attr = 0o100644 << 16
            archive.writestr(entry, payload)
    return {"item_id": ITEM, "version": manifest["version"], "sha256": hashlib.sha256(output.read_bytes()).hexdigest()}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("Store API redirects are refused")


class Store:
    def __init__(self, publisher: str, token: str):
        if not re.fullmatch(r"[A-Za-z0-9_-]+", publisher) or not token:
            raise ValueError("publisher ID and access token are required")
        self.name = f"publishers/{publisher}/items/{ITEM}"
        self.token = token

    def request(self, action: str, payload: bytes | None = None, *, upload: bool = False) -> dict[str, Any]:
        prefix = "/upload/v2/" if upload else "/v2/"
        request = urllib.request.Request(
            f"{API}{prefix}{self.name}:{action}",
            data=payload,
            headers={
                "Authorization": f"Bearer {self.token}",
                "Content-Type": "application/zip" if upload else "application/json",
            },
            method="GET" if payload is None else "POST",
        )
        try:
            with urllib.request.build_opener(NoRedirect).open(request, timeout=60) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            # A provider error body can echo request data; retain the status without the bearer token.
            raise ValueError(f"Store {action} failed with HTTP {error.code}") from None

    def status(self) -> dict[str, Any]:
        status = self.request("fetchStatus")
        if status.get("name") != self.name or status.get("itemId") != ITEM:
            raise ValueError("Store response identifies another item")
        return status

    def verify(self, version: str) -> dict[str, Any]:
        status = self.status()
        revision = status.get("publishedItemRevisionStatus", {})
        channels = revision.get("distributionChannels", [])
        if (
            status.get("takenDown")
            or status.get("warned")
            or revision.get("state") != "PUBLISHED"
            or not channels
            or any(c.get("crxVersion") != version or c.get("deployPercentage") != 100 for c in channels)
        ):
            raise ValueError("the required extension version is not published to every user")
        return status

    def submit(self, package: Path, version: str) -> dict[str, Any]:
        with ZipFile(package) as archive:
            manifest = json.loads(archive.read("manifest.json"))
        if "key" in manifest or manifest.get("version") != version:
            raise ValueError("package version differs or contains a development key")
        status = self.status()
        if status.get("submittedItemRevisionStatus"):
            raise ValueError("an existing submission must be resolved before uploading")
        published = status.get("publishedItemRevisionStatus", {})
        if any(channel.get("crxVersion") == version for channel in published.get("distributionChannels", [])):
            raise ValueError("this version is already published; bump the manifest version before submitting changes")
        upload = self.request("upload", package.read_bytes(), upload=True)
        state = upload.get("uploadState")
        deadline = time.monotonic() + 300
        while state == "IN_PROGRESS" and time.monotonic() < deadline:
            time.sleep(10)
            state = self.status().get("lastAsyncUploadState")
        if state != "SUCCEEDED":
            raise ValueError(f"upload did not succeed: {state}")
        response = self.request(
            "publish",
            json.dumps({"publishType": "STAGED_PUBLISH", "skipReview": False, "blockOnWarnings": True}).encode(),
        )
        if response.get("state") not in {"PENDING_REVIEW", "STAGED"}:
            raise ValueError("Store did not accept a staged review submission; inspect status before retrying")
        revision = self.status().get("submittedItemRevisionStatus", {})
        channels = revision.get("distributionChannels", [])
        if not channels or any(channel.get("crxVersion") != version for channel in channels):
            raise ValueError("Store submission version differs; inspect status before retrying")
        return response

    def publish(self, version: str) -> dict[str, Any]:
        status = self.status()
        revision = status.get("submittedItemRevisionStatus", {})
        channels = revision.get("distributionChannels", [])
        if revision.get("state") != "STAGED" or not channels or any(c.get("crxVersion") != version for c in channels):
            raise ValueError("the requested version is not approved and staged")
        response = self.request("publish", b'{"publishType":"DEFAULT_PUBLISH","blockOnWarnings":true}')
        if response.get("state") != "PUBLISHED":
            raise ValueError("Store did not confirm publication; inspect status before retrying")
        return self.verify(version)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("build", "status", "submit", "publish", "verify"))
    parser.add_argument("--package", type=Path, default=Path("dist/fastbrowse-extension.zip"))
    parser.add_argument("--version")
    parser.add_argument("--receipt", type=Path)
    args = parser.parse_args()
    if args.action == "build":
        result = build(SOURCE, args.package)
    else:
        store = Store(os.environ.get("CWS_PUBLISHER_ID", ""), os.environ.get("CWS_ACCESS_TOKEN", ""))
        if args.action == "status":
            result = store.status()
        elif not args.version:
            parser.error("--version is required for Store writes")
        elif args.action == "submit":
            if args.receipt:
                receipt = json.loads(args.receipt.read_text())
                if receipt != {
                    "item_id": ITEM,
                    "version": args.version,
                    "sha256": hashlib.sha256(args.package.read_bytes()).hexdigest(),
                }:
                    raise ValueError("package does not match the build receipt")
            result = store.submit(args.package, args.version)
        elif args.action == "verify":
            result = store.verify(args.version)
        else:
            result = store.publish(args.version)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
