"""An address read for the search state a page keeps in it: its query and its fragment alike.

Everything that asks whether two addresses name the same search reads them through here. Lineage once dropped the
fragment while the search check kept it, so on a site routing its searches by fragment a corrected `/search#new`
matched a guessed `/search#old` and stayed guessed.
"""

from urllib.parse import SplitResult, urlsplit


def search_parts(url: str) -> SplitResult:
    return urlsplit(url)


def search_address(url: str) -> str:
    """The address with its query and fragment, normalised as `search_parts` splits it."""
    return search_parts(url).geturl()


def searched(url: str) -> bool:
    """Whether the address carries search state, in its query or its fragment."""
    parts = search_parts(url)
    return bool(parts.query or parts.fragment)
