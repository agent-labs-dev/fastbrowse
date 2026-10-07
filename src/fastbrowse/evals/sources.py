"""Pinned source identities checked by the public publication gate."""

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field


class Source(BaseModel):
    model_config = ConfigDict(frozen=True)

    id: Literal["online-mind2web", "windtunnel"]
    upstream: str
    revision: str = Field(pattern=r"^[a-f0-9]{40}$")
    url: str
    sha256: str | None = Field(default=None, pattern=r"^[a-f0-9]{64}$")
    git_blob: str | None = None


OM2W_REVISION = "eacad896a84dc5b65e29b0b06e4699ab0544d701"
WINDTUNNEL_REVISION = "5ca8644e23826ebb30108e7bad240b61043bfe67"
SOURCES = {
    "online-mind2web": Source(
        id="online-mind2web",
        upstream="https://huggingface.co/datasets/osunlp/Online-Mind2Web",
        revision=OM2W_REVISION,
        url=f"https://huggingface.co/datasets/osunlp/Online-Mind2Web/resolve/{OM2W_REVISION}/Online_Mind2Web.json",
        git_blob="e8a5e5a99f2be9eae14f4e4259bd5af562f80da9",
    ),
    "windtunnel": Source(
        id="windtunnel",
        upstream="https://github.com/nekuda-ai/WindTunnel",
        revision=WINDTUNNEL_REVISION,
        url=f"https://codeload.github.com/nekuda-ai/WindTunnel/tar.gz/{WINDTUNNEL_REVISION}",
        sha256="9254737b8062a4a7140ec2a91f3b111abe648bd6d649b2775d2e68ad1f054d82",
    ),
}
