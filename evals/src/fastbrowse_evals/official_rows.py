"""Check one private official run and write the public rows `publish-candidate` merges.

Run it where the run directory is plaintext, after `track-official --push`. It holds the run to everything publication
requires, confirms Langfuse stored it, and writes only `publication.OfficialRows`. The output carries no task text,
answer, rubric or upstream record, so unlike the run directory it may leave the job unencrypted."""

import argparse
import asyncio
from pathlib import Path

from parallax.track import sink

from fastbrowse_evals import publication


def write_rows(group_id: str, source: publication.OfficialSource, out: Path) -> None:
    rows, tracked = asyncio.run(publication.official_group(group_id, source))
    client, project = sink.connect(write=False)
    try:
        publication.confirm_langfuse(client, project, tracked)
    finally:
        client.shutdown()
    out.write_text(rows.model_dump_json(indent=2) + "\n")


def main(argv: list[str]) -> None:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals browser official-rows", description=__doc__)
    parser.add_argument("group", choices=sorted(publication.OFFICIAL))
    parser.add_argument("folder", type=Path, help="the private official run directory")
    parser.add_argument("--dataset", type=Path, required=True)
    parser.add_argument("--upstream-root", type=Path, required=True)
    parser.add_argument("--judge-python", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    source = publication.OfficialSource(args.folder, args.dataset, args.upstream_root, args.judge_python)
    try:
        write_rows(args.group, source, args.out)
    except ValueError as exc:
        raise SystemExit(f"official-rows refused: {exc}") from exc
