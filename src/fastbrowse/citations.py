"""Links derived from captured quotes, without asking a model for a URL."""

import os
import re
from urllib.parse import quote

# A cited claim's link as the composer writes it: `[3](<https://page#:~:text=...>)`.
ANSWER_LINK = re.compile(r"\]\(<([^>]*)>\)")


def text_fragment(url: str, text: str) -> str:
    words = text.split()
    if not words:
        raise ValueError("A text fragment needs a nonempty quote")
    normalized = " ".join(words)
    # Long quotes make unwieldy URLs; keep whole words at both ends without overlapping the terms.
    terms = (" ".join(words[:5]), " ".join(words[-5:])) if len(normalized) > 120 and len(words) > 10 else (normalized,)
    # urllib leaves hyphens unescaped even with safe="", but the text directive uses them as syntax.
    directive = ",".join(quote(term, safe="").replace("-", "%2D") for term in terms)
    base, _, fragment = url.partition("#")
    anchor = fragment.partition(":~:")[0]
    return f"{base}#{anchor}:~:text={directive}"


# The Rust core (crates/fastbrowse-py) in place of the function above, while the port is checked against
# these tests; the default stays Python.
if os.environ.get("FASTBROWSE_RUST_CORE") == "1":
    from fastbrowse._native import text_fragment  # ty: ignore[unresolved-import]  # noqa: F401
