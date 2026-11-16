"""Sign the evidence-free campaign report produced by Parallax without --reasons."""

import argparse
import hashlib
import hmac
import os
import sys
from pathlib import Path
from typing import Literal
from urllib.request import Request, urlopen

from pydantic import BaseModel, ValidationError


class Notification(BaseModel):
    source: Literal["fastbrowse-benchmark-campaign"] = "fastbrowse-benchmark-campaign"
    repository: str
    campaign_id: str
    group: str
    status: Literal["success", "failure", "cancelled", "skipped"]
    smoke: bool
    run_url: str
    report_markdown: str


def send(report: str) -> None:
    url, secret = os.environ.get("WEBHOOK_URL"), os.environ.get("WEBHOOK_SECRET")
    if not url or not secret:
        return
    payload = Notification.model_validate(
        {
            "repository": os.environ["GITHUB_REPOSITORY"],
            "campaign_id": os.environ["CAMPAIGN_ID"],
            "group": os.environ["GROUP"],
            "status": os.environ["STATUS"],
            "smoke": os.environ["SMOKE"].lower() == "true",
            "run_url": os.environ["RUN_URL"],
            "report_markdown": report,
        }
    )
    body = payload.model_dump_json().encode()
    signature = hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    request = Request(url, data=body, headers={"Content-Type": "application/json", "X-Webhook-Signature": signature})
    with urlopen(request, timeout=10):
        pass


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("report", type=Path)
    args = parser.parse_args(argv)
    try:
        send(args.report.read_text(encoding="utf-8"))
    except (OSError, ValueError, KeyError, ValidationError):
        # HTTP errors may include the secret-bearing destination or response body.
        print("::warning::campaign report notification failed", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
