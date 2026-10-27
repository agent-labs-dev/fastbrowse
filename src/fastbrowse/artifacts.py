"""A filesystem `ArtifactSink`. Hosts with their own storage implement the protocol instead."""

import hashlib
from pathlib import Path

from fastbrowse.models import Artifact, ArtifactKind


class DirectorySink:
    def __init__(self, root: Path) -> None:
        self._root = root.resolve()

    @property
    def root(self) -> Path:
        return self._root

    async def put(self, kind: ArtifactKind, name: str, mime_type: str, content: bytes) -> Artifact:
        digest = hashlib.sha256(content).hexdigest()
        # Content-addressed so a hostile filename can neither escape the root nor overwrite another artifact.
        path = self._root / kind.value / digest[:16] / (Path(name).name or "artifact")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return Artifact(
            kind=kind, name=name, mime_type=mime_type, size_bytes=len(content), sha256=digest, uri=path.as_uri()
        )
